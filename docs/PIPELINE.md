# Le pipeline transactionnel

Un scan traverse cinq phases. **Rien n'est écrit dans la librairie avant la phase 5.** C'est
l'invariant central : toute modification qui écrirait dans la librairie depuis une autre
phase est un bug, pas une optimisation.

```text
PHASE 1  INVENTAIRE      séquentiel   parcours, élagage, filtres, caches
   │                                  → candidats
PHASE 2  PLANIFICATION   parallèle    date + modèle + destination voulue
   │                     LECTURE SEULE → plans
PHASE 3  RÉSOLUTION      séquentiel   doublons écartés, noms finaux alloués
   │                     EN MÉMOIRE   → fichiers résolus
PHASE 4  PRÉPARATION     parallèle    matérialisation dans la zone de travail + EXIF
   │                     TRAVAIL SEUL → entrées prêtes (+ journal)
PHASE 5  COMMIT          séquentiel   5a copie sous un nom caché dans la librairie (lent, invisible)
                                      5b liaison atomique vers le vrai nom (rafale rapide)
```

Orchestration : [src/scanner.rs](../src/scanner.rs).

## Pourquoi

L'ancien programme déplaçait les fichiers un par un pendant le scan. Immich indexe la même
arborescence en parallèle : il a indexé des chemins qui disparaissaient quelques secondes
plus tard (fichiers `isOffline`, échecs `ENOENT`), et les suffixes `_N` parasites venaient
d'un `exists()` suivi, bien plus tard, d'un `rename()`.

Ici tout le travail se fait **hors de la librairie**, dans une zone privée. La librairie ne
reçoit que le résultat final : les copies se font sous un nom caché que les scanners
n'indexent pas, puis tous les fichiers prennent leur vrai nom d'un coup. La fenêtre pendant
laquelle un fichier peut apparaître puis disparaître se réduit à cette dernière rafale
d'opérations de métadonnées.

**Pourquoi Immich ne voit pas les noms temporaires.** Immich parcourt la librairie avec le
motif `<chemin>/**/*{extensions média supportées}` : seuls les noms qui *finissent* par une
extension média connue sont candidats. `.IMG_1.jpg.mo-part` finit par `.mo-part` (et est de
plus un fichier caché). Aucune exclusion n'est à configurer dans Immich.

## Les phases

**1 — Inventaire** ([`Scanner::inventory`](../src/scanner.rs)). `WalkDir` avec
`filter_entry` : les dossiers exclus (`@eaDir`, `#recycle`…) sont **élagués**, jamais
descendus — sur un volume DSM monté en NFS, `@eaDir` contient une vignette par photo, et
la parcourir pour la jeter coûterait un aller-retour réseau chacune. Sont aussi écartés :
extensions non autorisées, fichiers modifiés depuis moins de `stability_secs`, fichiers
du cache « sans date », doublons déjà connus (si leur mtime n'a pas changé), et la zone de
transit elle-même.

**2 — Planification** ([src/plan.rs](../src/plan.rs)). Pour chaque candidat : la date et sa
source (chaîne ci-dessous), le modèle d'appareil, la destination relative issue du motif,
et si la date doit être écrite dans l'EXIF. Strictement en lecture. Chaque fichier est
isolé par `catch_unwind` : une panique ne tue pas le lot.

**3 — Résolution** ([src/resolve.rs](../src/resolve.rs)). Les plans sont triés par chemin
source (résultat reproductible), puis chaque nom final est alloué depuis **une seule table
en mémoire** : il n'y a plus de course `exists()`/`rename()`. Si un nom est pris :

- même taille *et* même empreinte → **doublon** : la source reste en place, est mémorisée.
  Avec `dedup.delete_source_duplicates`, la source d'un doublon dont l'original est déjà sur
  disque est supprimée en phase 5 après une relecture octet par octet des deux fichiers ;
  un doublon du même lot attend le scan suivant ;
- sinon `on_conflict` : `rename` essaie `_1`, `_2`… ; `skip` laisse la source ; `overwrite`
  remplace un fichier **déjà sur disque** (jamais un fichier du même lot : ce serait en
  perdre un).

On ne hache que les fichiers dont la taille collisionne : si toutes les tailles diffèrent,
**aucun hachage n'est calculé**.

**4 — Préparation** ([src/staging.rs](../src/staging.rs), `Staging::prepare`). Chaque
fichier est matérialisé dans la zone de travail privée :

| Cas | Opération |
| --- | --- |
| source et zone de travail sur le même montage | `hard_link` — gratuit |
| injection EXIF nécessaire | `copy` puis écriture du tag : on ne touche jamais l'inode de la source |
| montages différents (cas normal avec un volume par dossier) | `copy` |

**La source n'est donc jamais réécrite**, contrairement à l'ancienne version.

**5 — Commit** (`Staging::commit`), en deux temps :

- **5a — placement** (lent, invisible). Pour chaque entrée : création de la chaîne de
  répertoires dans la librairie (avec `dir_mode` sur *chacun*, de la racine de la librairie
  au parent, mais seulement si le mode diffère ; un `chmod` refusé est toléré), puis le fichier est déplacé (`rename`, gratuit sur un même montage) ou **copié**
  (montages différents) à côté de sa destination sous le nom `.<nom>.mo-part`, avec `file_mode`
  déjà appliqué.
- **5b — publication** (rafale rapide). Une fois *tout* placé, chaque fichier reçoit son vrai
  nom par `link()`, atomique et qui **échoue si le nom existe** : un écrivain **externe**
  (ex. Nextcloud déposant un fichier en cours de scan) n'est jamais écrasé — on passe au
  suffixe suivant. Le fichier est complet à l'instant où il apparaît. Sur un système de
  fichiers sans liens physiques, repli sur une réservation `create_new` puis `rename`.

La source n'est supprimée qu'**après** la publication.

Un échec est journalisé et **le lot continue** : l'entrée reste dans le journal, réessayée au
scan suivant. Pas de rollback global — un seul fichier corrompu ne doit jamais bloquer toute
progression.

## Après la phase 5 : l'annonce du lot

Quand le pipeline d'un dossier est terminé, ce qu'il a changé est **annoncé** (module
[`notify`](../src/notify/mod.rs), voir [DEPLOYMENT.md](DEPLOYMENT.md#annonce-de-fin-de-lot-notify)).
Le scanner accumule, au fil des phases, les fichiers publiés (replay, commit) et les sources
de doublons supprimées, et les remet au notifieur à la fin du dossier — à chaque scan, même
sans rien de neuf. Le notifieur les **retient** (les uploads arrivent au fil de l'eau) et
publie un seul événement quand l'input est resté calme `quiet_secs`, ou `max_wait_secs` après
le premier fichier retenu. C'est la seule
écriture hors des phases 4 et 5, et elle ne va **jamais** dans une librairie ni un input
(`notify.dir` est validé hors de tous les deux). Elle n'existe pas en `--dry-run`, et son
échec n'est qu'un `warn`.

## La chaîne de date

Dans cet ordre exact ([src/date_reader.rs](../src/date_reader.rs)) :

| Rang | Source | Confiance | Journal |
| --- | --- | --- | --- |
| 1 | EXIF `DateTimeOriginal` (photos) | haute | `debug` |
| 2 | QuickTime `mvhd.creation_time` (vidéos) | haute | `debug` |
| 3 | motif du nom de fichier | bonne | `debug` |
| 4 | **mtime du fichier** | **faible — une approximation** | **`warn`** |

Ne jamais remonter le mtime avant le nom de fichier. Le mtime ne déclenche **pas**
d'injection EXIF par défaut (`write_exif_from_mtime = false`). `/healthz` compte les
sources séparément : un pic de `mtime` signifie que les fichiers entrants perdent leurs
métadonnées quelque part en amont.

## Zone de travail

Privée au service : `<state_dir>/work/<nom du dossier>/` par défaut (réglable avec
`work_dir`). Elle contient les fichiers en cours de traitement et le journal. **Elle n'a pas
besoin d'être montée**, sauf pour déboguer. Elle est volontairement *hors* de la librairie :
Immich ne voit jamais de copie intermédiaire. Chaque `[[folders]]` a son propre sous-dossier,
d'où l'unicité obligatoire des noms.

Elle peut vivre sur n'importe quel système de fichiers : si ce n'est pas celui de la librairie,
la phase 5a copie au lieu de déplacer.

## Journal de reprise

`<zone de travail>/journal.jsonl`, une ligne JSON par entrée, écrit **atomiquement avant** le commit :

```json
{"staged":"/…/staging/9f2c…e1.jpg","final_dest":"/…/2025/06/2025-06-26_…jpg",
 "source":"/…/synchronisation/IMG-20250626-WA0002.jpg","overwrite":false,
 "file_mode":438,"dir_mode":511,"date_source":"filename","injected_exif":true}
```

(`file_mode`/`dir_mode` sont les entiers décimaux de 0o666 / 0o777 ; `null` = pas de chmod.)

Rejoué au démarrage de chaque scan (et par `media-organizer recover`) :

- entrée dont le fichier de travail, ou le fichier temporaire `.mo-part` déjà placé dans la
  librairie, existe encore → committée ;
- entrée où il n'y a plus ni l'un ni l'autre → déjà publiée, abandonnée ;
- fichier de travail sans ligne de journal (coupure *pendant* la préparation) → purgé ; ses
  sources n'ont jamais été touchées et seront refaites.

Un nombre d'entrées « en attente » non nul de façon persistante (`pending_in_staging` dans
`/healthz`) signale un commit qui échoue en boucle.

## État persistant (`state_dir`)

| Fichier | Contenu |
| --- | --- |
| `no_date_cache.json` | fichiers qu'aucune source n'a pu dater (clé : chemin, invalidé si le mtime change) |
| `duplicates.json` | sources connues pour dupliquer un fichier rangé |
| `hash_index.json` | empreintes déjà calculées (clé : chemin, valide tant que taille et mtime sont inchangés) |
| `notify.json` | par dossier : dernier événement annoncé (`seq`, date, compteurs) et changements **retenus** en attente d'annonce. **Pas un cache** : le perdre fait oublier ce qui était retenu (le `seq` repart du fichier d'événement) |

Les trois premiers sont des caches : un fichier absent ou corrompu repart vide, sans empêcher le démarrage.

## Parallélisme

`rayon`, pool dédié (`workers`, défaut 8 ; 1 = séquentiel déterministe). **Seules les
phases 2 et 4 sont parallèles, et aucune ne mute d'état partagé** — c'est pourquoi les
caches n'ont aucun `Mutex`. Pas de `tokio` : tout l'IO est un appel système bloquant sur NFS.

`catch_unwind` isole chaque fichier : **n'ajoutez jamais `panic = "abort"`**.

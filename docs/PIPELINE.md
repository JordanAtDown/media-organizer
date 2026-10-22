# Le pipeline transactionnel

Un scan traverse cinq phases. **Rien n'est écrit dans `output` avant la phase 5.** C'est
l'invariant central : toute modification qui écrirait dans `output` depuis une autre phase
est un bug, pas une optimisation.

```text
PHASE 1  INVENTAIRE      séquentiel   parcours, élagage, filtres, caches
   │                                  → candidats
PHASE 2  PLANIFICATION   parallèle    date + modèle + destination voulue
   │                     LECTURE SEULE → plans
PHASE 3  RÉSOLUTION      séquentiel   doublons écartés, noms finaux alloués
   │                     EN MÉMOIRE   → fichiers résolus
PHASE 4  PRÉPARATION     parallèle    matérialisation en zone de transit + EXIF
   │                     TRANSIT SEUL → entrées prêtes (+ journal)
PHASE 5  COMMIT          séquentiel   rename(transit → destination), modes, unlink source
                         RAPIDE       → une rafale d'opérations de métadonnées
```

Orchestration : [src/scanner.rs](../src/scanner.rs).

## Pourquoi

L'ancien programme déplaçait les fichiers un par un pendant le scan. Immich indexe la même
arborescence en parallèle : il a indexé des chemins qui disparaissaient quelques secondes
plus tard (fichiers `isOffline`, échecs `ENOENT`), et les suffixes `_N` parasites venaient
d'un `exists()` suivi, bien plus tard, d'un `rename()`.

Ici la fenêtre pendant laquelle un fichier peut apparaître puis disparaître se réduit à la
durée de la phase 5 : une rafale de `rename()`, opérations de métadonnées sur un même
système de fichiers.

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

- même taille *et* même empreinte → **doublon** : la source reste en place, est mémorisée ;
- sinon `on_conflict` : `rename` essaie `_1`, `_2`… ; `skip` laisse la source ; `overwrite`
  remplace un fichier **déjà sur disque** (jamais un fichier du même lot : ce serait en
  perdre un).

On ne hache que les fichiers dont la taille collisionne : si toutes les tailles diffèrent,
**aucun hachage n'est calculé**.

**4 — Préparation** ([src/staging.rs](../src/staging.rs), `Staging::prepare`). Chaque
fichier est matérialisé dans la zone de transit :

| Cas | Opération |
| --- | --- |
| cas courant | `hard_link` — gratuit, `input` et `output` sont sous le même export |
| injection EXIF nécessaire | `copy` puis écriture du tag : on ne touche jamais l'inode de la source |
| `hard_link` impossible | repli sur `copy` |

**La source n'est donc jamais réécrite**, contrairement à l'ancienne version.

**5 — Commit** (`Staging::commit`). Pour chaque entrée : création de la chaîne de
répertoires (et `dir_mode` sur *chacun* jusqu'à `output`), réservation du nom par
`create_new` (garde contre un écrivain **externe**, ex. Nextcloud déposant un fichier en
cours de scan), `rename(transit → destination)`, `file_mode`, puis suppression de la source.
La source n'est supprimée qu'**après** la publication.

Un échec est journalisé et **le lot continue** : l'entrée reste en transit et dans le
journal, réessayée au scan suivant. Pas de rollback global — un seul fichier corrompu ne
doit jamais bloquer toute progression.

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

## Zone de transit

Par défaut `<output>/.media-organizer/staging/`. **Elle doit être sur le même système de
fichiers que `output`** — vérifié (`st_dev`) à la validation de la configuration puis à chaque scan, car sinon le commit redevient
une copie inter-systèmes. Elle est élaguée du parcours.

## Journal de reprise

`staging/journal.jsonl`, une ligne JSON par entrée, écrit **atomiquement avant** le commit :

```json
{"staged":"/…/staging/9f2c…e1.jpg","final_dest":"/…/2025/06/2025-06-26_…jpg",
 "source":"/…/synchronisation/IMG-20250626-WA0002.jpg","overwrite":false,
 "file_mode":438,"dir_mode":511,"date_source":"filename","injected_exif":true}
```

(`file_mode`/`dir_mode` sont les entiers décimaux de 0o666 / 0o777 ; `null` = pas de chmod.)

Rejoué au démarrage de chaque scan (et par `media-organizer recover`) :

- entrée dont le fichier de transit existe → committée ;
- entrée dont le fichier de transit a disparu → déjà publiée, abandonnée ;
- fichier de transit sans ligne de journal (coupure *pendant* la préparation) → purgé ; ses
  sources n'ont jamais été touchées et seront refaites.

Un nombre d'entrées « en attente » non nul de façon persistante (`pending_in_staging` dans
`/healthz`) signale un commit qui échoue en boucle.

## État persistant (`state_dir`)

| Fichier | Contenu |
| --- | --- |
| `no_date_cache.json` | fichiers qu'aucune source n'a pu dater (clé : chemin, invalidé si le mtime change) |
| `duplicates.json` | sources connues pour dupliquer un fichier rangé |
| `hash_index.json` | empreintes déjà calculées (clé : chemin, valide tant que taille et mtime sont inchangés) |

Ce sont des caches : un fichier absent ou corrompu repart vide, sans empêcher le démarrage.

## Parallélisme

`rayon`, pool dédié (`workers`, défaut 8 ; 1 = séquentiel déterministe). **Seules les
phases 2 et 4 sont parallèles, et aucune ne mute d'état partagé** — c'est pourquoi les
caches n'ont aucun `Mutex`. Pas de `tokio` : tout l'IO est un appel système bloquant sur NFS.

`catch_unwind` isole chaque fichier : **n'ajoutez jamais `panic = "abort"`**.

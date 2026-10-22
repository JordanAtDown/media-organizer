# Guide d'architecture

Écrit depuis le code existant. Si le code et ce fichier divergent, **le code a raison** :
corrigez ce fichier dans le même commit que le changement. L'ancien dépôt a vu sa doc
décrire des mécanismes qui n'existaient pas (un watcher inotify, un pool plafonné) — ne
refaites pas cette erreur.

## Invariants — ne pas casser

1. **Le pipeline est transactionnel en 5 phases** (inventaire, planification, résolution,
   préparation, commit). **Rien n'est écrit dans la librairie avant la phase 5.** Toute
   modification qui écrirait dans la librairie depuis une autre phase est un bug, pas une
   optimisation. Voir [docs/PIPELINE.md](../docs/PIPELINE.md).
2. **La zone de travail est privée et hors de la librairie** (`<state_dir>/work/<dossier>`) :
   Immich ne doit jamais voir de copie intermédiaire. La phase 5 place le fichier sous un
   nom caché `.<nom>.mo-part` (extension non média, donc jamais indexé) puis le publie par
   `link()` — atomique, n'écrase jamais, le fichier apparaît complet. Ne jamais publier un
   fichier sous son vrai nom avant qu'il soit complet.
3. **Le journal** (`<zone de travail>/journal.jsonl`) rend la reprise après coupure
   déterministe ; il est écrit atomiquement *avant* le commit et rejoué à chaque scan.
4. **Un doublon n'est jamais supprimé.** Même nom de destination + même taille + même
   empreinte BLAKE3 : la source reste en place et est mémorisée. On ne hache que si les
   tailles collisionnent.
5. **La source n'est jamais réécrite.** L'injection EXIF se fait sur la copie dans la zone de travail ;
   la source n'est supprimée qu'après publication.
6. **La chaîne de date est EXIF → QuickTime → nom de fichier → mtime, dans cet ordre exact.**
   Le mtime est un dernier recours dont la date est une *approximation* : `warn`, compté à
   part dans `/healthz`, et **pas** d'injection EXIF par défaut. Ne jamais remonter le mtime
   avant le nom de fichier.
7. **Ne jamais changer la sémantique de date d'un format déjà pris en charge** : cela
   déplacerait des fichiers déjà rangés (un décalage d'une heure fracture la photothèque au
   changement de mois). `mvhd.creation_time` est une valeur brute d'époque Mac, `== 0`
   rejeté (vignettes `SYNOPHOTO_FILM_M.mp4` de DSM).
8. **Seules les phases 2 et 4 sont parallèles** (`rayon`), et aucune ne mute d'état
   partagé : c'est pourquoi les caches n'ont pas de `Mutex`. Pas de `tokio` : tout l'IO est
   bloquant sur NFS.
9. **`catch_unwind` autour de chaque fichier, donc jamais `panic = "abort"`.**
10. **C'est du polling**, pas de l'inotify : NFS ne propage pas les événements.
11. **`TZ` est fonctionnel.** Toutes les dates sont des `DateTime<Local>` ; sans `tzdata`/`TZ`
    elles deviennent UTC et les fichiers changent de dossier. Les tests comparent en `Utc`.

## Pièges NAS à ne surtout pas « simplifier »

Le NAS n'exécute plus rien, mais il reste le **magasin de données** (volume DSM exporté en
NFS) :

- les `excluded_dirs` Synology (`@eaDir`, `#recycle`, `#snapshot`…) : une vignette *par
  photo* y est toujours générée. L'élagage (`filter_entry`) est le gain de perf le plus
  direct — ne pas revenir à un filtre a posteriori ;
- `file_mode` / `dir_mode` : l'export est en `all_squash` et Nextcloud répond 403 pour un
  fichier illisible. Le mode s'applique à **toute la chaîne** de répertoires jusqu'à la racine de la librairie,
  pas seulement au dernier. `set_permissions` ignore le umask ;
- `Mode` est une **chaîne octale** (`"0777"`), jamais un entier TOML (lu en décimal) ;
- le parseur ISOBMFF ne va que de `moov` à `mvhd` : les parseurs d'arbre complet échouent
  silencieusement sur les boîtes Apple (`tapt`, `clef`, `prof`, `enof`).

## Règles de discipline

1. Après chaque fonctionnalité ou correctif, vérifier que `README.md`, `config.example.toml`
   **et** `docs/DEPLOYMENT.md` sont à jour. `DEPLOYMENT.md` est le contrat que lira le dépôt
   d'infrastructure.
2. Vocabulaire : **inputs** (sources), **libraries** (destinations finales, l'ancien
   `output`), **work** (zone de travail interne). Ne pas réintroduire `output`.
3. Toute nouvelle option de configuration apparaît dans `config.example.toml` **et** est
   couverte par un test de validation. `deny_unknown_fields` est partout : le fichier est
   templaté par Ansible, une faute de frappe doit échouer bruyamment.
4. Toute modification du pipeline précise **dans quelle phase** elle intervient. Une écriture
   hors phases 4/5 est refusée en revue.
5. **Fixtures binaires** : admises uniquement comme extraits de métadonnées sans charge média,
   sous ~10 Ko, avec provenance, commande d'extraction et valeur attendue documentées dans
   `tests/fixtures/README.md`. Sinon, générer à l'exécution (`tests/common/mod.rs`).
6. Erreurs typées `thiserror` par module ; `anyhow` **uniquement** dans `main.rs`.
7. Les tests d'intégration utilisent `--once`/`Scanner::scan_once` : jamais de `sleep` pour
   attendre le démon.
8. Une dépendance de plus se justifie. `Cargo.lock` est commité. `cargo audit` : les
   advisories acceptées sont dans `.cargo/audit.toml`, avec raison et condition de sortie.
9. Documentation en français ; commits et commentaires de code en anglais.

## Ce qui est volontairement absent

- **Pas de multiplateforme** : image `linux/arm64` seulement, optimisée pour le Pi 5. Corollaire
  assumé : elle ne tourne pas sur une VM de staging x86_64 (voir `docs/DEPLOYMENT.md`).
- **Pas de tag `latest`** : l'infrastructure épingle toujours une version exacte.
- Le rôle Ansible et tout le dépôt d'infrastructure sont hors périmètre ici.
- Pas de suppression automatique des doublons, pas de rétro-déduplication de l'existant.

## Commandes utiles

```bash
cargo test                                   # unitaires (src/) + intégration (tests/)
cargo run -- -c config.toml --dry-run --once # plan complet, rien d'écrit
cargo run -- -c config.toml validate-config  # valide et affiche la config effective
cargo audit
```

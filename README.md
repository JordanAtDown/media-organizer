# media-organizer

[![CI](https://github.com/JordanAtDown/media-organizer/actions/workflows/ci.yml/badge.svg)](https://github.com/JordanAtDown/media-organizer/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/JordanAtDown/media-organizer)](https://github.com/JordanAtDown/media-organizer/releases)

Range automatiquement photos et vidéos dans une arborescence datée (`ANNÉE/MOIS`) à
partir de leurs métadonnées, **de façon transactionnelle**. Conçu pour tourner en
**conteneur sur un Raspberry Pi 5** qui monte un NAS en NFS.

Reprise de `syno-media-organizer` (qui tournait en paquet DSM sur le NAS) : même logique
métier éprouvée, architecture refaite pour le Pi.

```text
<nextcloud>/<email>/files/synchronisation          (dépôt brut, auto-upload du téléphone)
        │  media-organizer : date → nom → doublons → rangement
        ▼
<nextcloud>/<email>/files/photos/phototeque/YYYY/MM/…   (bibliothèque externe Immich)
```

---

## Fonctionnalités

- Scanne un ou plusieurs dossiers d'entrée toutes les N secondes (30 par défaut).
- **Photos** : date EXIF `DateTimeOriginal` (tag 0x9003).
- **Vidéos** : date de création QuickTime `mvhd` (UTC → heure locale).
- **Repli sur le nom de fichier** quand les métadonnées manquent : WhatsApp
  (`IMG-YYYYMMDD-WAxxxx`), appareil Android (`IMG_YYYYMMDD_HHMMSS`), Facebook
  (`FB_IMG_<timestamp>`), `Resized_…`.
- **Dernier recours : la date de dernière modification du fichier.** Plus aucun fichier ne
  reste bloqué dans le dossier de dépôt (désactivable : `use_mtime_fallback = false`).
- **Détection de doublons** : même nom de destination *et* même contenu. Le doublon reste en
  place, mémorisé, **jamais supprimé**.
- **Transactionnel** : tout est préparé dans une zone de transit, puis publié d'un coup. Une
  coupure en cours de route se rattrape au scan suivant (journal de reprise).
- Conflits de nom entre fichiers différents : `rename`, `skip` ou `overwrite`.
- Logs JSON sur stdout, arrêt propre sur SIGTERM, `/healthz` pour la supervision.

## Architecture

```text
toutes les poll_interval_secs (30 s)
   │
   1 INVENTAIRE      parcours, élagage des dossiers parasites, filtres
   2 PLANIFICATION   date (EXIF → QuickTime → nom → mtime) + destination      lecture seule
   3 RÉSOLUTION      doublons écartés, noms finaux alloués                     en mémoire
   4 PRÉPARATION     lien physique ou copie en zone de transit (+ EXIF)        transit seul
   5 COMMIT          rename() vers la destination, modes, suppression source   rafale rapide
```

**Rien n'est écrit dans `output` avant la phase 5.** Détails et invariants :
[docs/PIPELINE.md](docs/PIPELINE.md).

---

## Déploiement : récupérer l'image et sa version

Ce dépôt **fabrique et publie** l'image ; il ne la déploie pas. Le circuit :

```text
 ce dépôt                          GitHub Container Registry            le Raspberry Pi
 ─────────                         ─────────────────────────            ───────────────
 scripts/release.sh 1.2.0   ──►    ghcr.io/jordanatdown/       ──►     docker pull / compose up
 git push origin main v1.2.0       media-organizer:v1.2.0              (version choisie côté infra)
   └─ workflow « Release »         publique, sans identifiants
      construit l'image arm64
```

**Récupérer l'image.** Elle est **publique** : n'importe quelle machine Docker arm64 la tire
sans identifiants, sans secret de registre.

```bash
docker pull ghcr.io/jordanatdown/media-organizer:v1.0.0
```

En pratique on ne la tire pas à la main : le fichier `docker-compose` du service la nomme
(`image: ghcr.io/jordanatdown/media-organizer:v1.0.0`) et `docker compose pull` la
récupère. Exemple complet : [docker/docker-compose.example.yml](docker/docker-compose.example.yml).

**Qui porte le numéro de version ?** Deux endroits, avec deux rôles différents :

| Où | Rôle |
| --- | --- |
| **ce dépôt** : `Cargo.toml`, tag git `vX.Y.Z`, section du `CHANGELOG.md` | *produit* la version. `scripts/release.sh` garde les trois d'accord. Le tag devient le tag de l'image, et la version s'affiche dans `--version` et `/healthz` |
| **le dépôt d'infrastructure** (celui qui contient vos rôles Ansible, par ex. une variable `media_organizer_version` dans `group_vars/all.yml`) | *choisit* quelle version tourne sur le Pi |

Publier une nouvelle version **ne déploie rien** : le Pi continue de faire tourner la
version épinglée. Pour migrer, on change le numéro dans le dépôt d'infrastructure puis on
relance le déploiement. Il n'y a **pas de tag `latest`** : une version se désigne toujours
exactement.

Publier une version : [RELEASING.md](RELEASING.md). Contrat complet de déploiement (volumes,
variables, supervision, première mise en service) : [docs/DEPLOYMENT.md](docs/DEPLOYMENT.md).

---

## Prérequis

| Élément | Valeur |
| --- | --- |
| Machine | Raspberry Pi 5 (**image `linux/arm64` uniquement**) |
| Stockage média | NAS monté en **NFS** ; `input` et `output` sous le **même export** |
| Moteur | Docker avec Compose |

---

## Démarrage rapide

```bash
cp config.example.toml config.toml      # adapter les chemins
docker run --rm -v "$PWD/config.toml:/etc/media-organizer/config.toml:ro" \
  ghcr.io/jordanatdown/media-organizer:vX.Y.Z validate-config
```

**Avant le premier passage en production, lancez `--dry-run`** : il exécute l'inventaire,
la planification et la résolution complets, affiche chaque fichier avec la source de sa
date et sa destination, et n'écrit rien.

```bash
docker run --rm … ghcr.io/jordanatdown/media-organizer:vX.Y.Z --dry-run --once
```

---

## Configuration

Un fichier TOML, entièrement annoté : [config.example.toml](config.example.toml). Toute clé
inconnue est refusée. Points à connaître :

- **`file_mode` / `dir_mode`** sont des *chaînes octales* (`"0777"`), pas des entiers.
- **`TZ` est obligatoire** dans le conteneur : sans lui, les fichiers changent de dossier
  (UTC au lieu de l'heure locale). Le fuseau détecté est journalisé au démarrage.
- La **zone de transit** doit être sur le même système de fichiers que `output`.

```toml
poll_interval_secs = 30

[[folders]]
input        = "/mnt/nas-nextcloud/utilisateur@exemple.org/files/synchronisation"
output       = "/mnt/nas-nextcloud/utilisateur@exemple.org/files/photos/phototeque"
pattern      = "{year}/{month}/{year}-{month}-{day}_{hour}{min}{sec}_{stem}{ext}"
recursive    = true
photo_prefix = "IMG_"        # optionnel, défaut : ""
video_prefix = "VID_"        # optionnel, défaut : ""
on_conflict  = "rename"      # rename | skip | overwrite
extensions   = ["jpg", "jpeg", "png", "heic", "mp4", "mov", "avi", "mkv"]
```

### Jetons du motif

| Jeton | Description |
| --- | --- |
| `{year}` | année sur 4 chiffres (2024) |
| `{month}` | mois sur 2 chiffres (01–12) |
| `{day}` | jour sur 2 chiffres (01–31) |
| `{hour}` | heure sur 2 chiffres (00–23) |
| `{min}` / `{sec}` | minute / seconde sur 2 chiffres |
| `{stem}` | nom d'origine sans extension |
| `{ext}` | extension avec le point (`.jpg`) |
| `{camera}` | modèle d'appareil EXIF, assaini (ou `unknown`) |
| `{prefix}` | `photo_prefix` pour une photo, `video_prefix` pour une vidéo |

> `{counter}` n'existe plus : il rendait toujours `0000`. Les collisions de nom sont gérées
> par `on_conflict`. Un motif qui l'utilise est refusé avec un message explicite.

### Dossiers exclus

`excluded_dirs` liste les noms de dossiers **élagués** du parcours : on n'y descend jamais
(comparés à chaque composant du chemin). Les données vivent toujours sur un volume
Synology exporté en NFS, qui continue de générer ces dossiers — dont une vignette `@eaDir`
**par photo** :

| Dossier | Contenu |
| --- | --- |
| `@eaDir` | vignettes et métadonnées (Synology Photos / Photo Station) |
| `@SynoEAStream` | flux d'attributs étendus |
| `@Recycle` / `#recycle` | corbeille des dossiers partagés (selon la version de DSM) |
| `#snapshot` | instantanés |
| `@tmp` | fichiers temporaires DSM |
| `.stfolder`, `.stversions`, `.Trash`, `$RECYCLE.BIN`, `System Volume Information` | clients de synchronisation et autres systèmes |

D'autres dossiers DSM peuvent s'ajouter selon votre installation : `@sharebin` (corbeille,
anciennes versions), `@database` et `@photos` (Synology Photos), `@Recently-Snapshot`
(Btrfs). Pour remplacer la liste, redéfinissez `excluded_dirs` dans le bloc `[[folders]]`.

### Fichiers sans date

Avec `use_mtime_fallback = true` (défaut), presque aucun fichier n'échoue. Si vous le passez
à `false`, un fichier qu'aucune source ne peut dater n'est **jamais déplacé** : il est
journalisé en `WARN` la première fois puis mémorisé (`state_dir/no_date_cache.json`) et
ignoré tant que son mtime ne change pas (ou après `no_date_cache.ttl_days`). Pour repartir
de zéro : supprimer ce fichier.

### Niveau de log

Priorité (la plus haute l'emporte) :

| Source | Exemple |
| --- | --- |
| variable `RUST_LOG` | `RUST_LOG=debug` |
| flag `--log-level` / `-v` (ou `MO_LOG_LEVEL`) | `--log-level trace` |
| `log_level` dans le fichier de config | `log_level = "debug"` |
| défaut | `"info"` |

Valeurs : `error`, `warn`, `info`, `debug`, `trace`. Utilisez `debug` pour comprendre
pourquoi un fichier est ignoré ou voir les détails de l'injection EXIF.

### Flags et variables d'environnement

Priorité : **flag CLI > variable `MO_*` > fichier de config > défaut**.

| Flag | Variable | Rôle |
| --- | --- | --- |
| `-c, --config` | `MO_CONFIG` | fichier de configuration |
| `--state-dir` | `MO_STATE_DIR` | caches persistants |
| `--log-format text\|json` | `MO_LOG_FORMAT` | l'image impose `json` |
| `--dry-run` | `MO_DRY_RUN` | planifie sans rien écrire |
| `--once` | `MO_ONCE` | un seul scan puis sortie |
| `--poll-interval-secs` | `MO_POLL_INTERVAL_SECS` | |
| `--workers` | `MO_WORKERS` | |
| `--health-bind` | `MO_HEALTH_BIND` | `""` désactive |

Sous-commandes : `validate-config` (valide et affiche la config effective),
`healthcheck` (sonde `/healthz`, utilisée par le `HEALTHCHECK` Docker), `recover` (rejoue
le journal de transit).

---

## Formats pris en charge

Seules les extensions listées dans `extensions` sont traitées : ajoutez celles que vous
voulez (`heif`, `tif`, `3gp`…).

### Photos — EXIF `DateTimeOriginal`

| Extension | Format |
| --- | --- |
| `.jpg` / `.jpeg` | JPEG |
| `.heic` / `.heif` | image haute efficacité (iPhone) |
| `.png` | PNG avec bloc EXIF |
| `.tiff` / `.tif` | TIFF |

Sans `DateTimeOriginal`, la date vient du nom de fichier, puis du mtime. L'injection de la
date dans l'EXIF (quand elle vient du nom) ne concerne que `jpg`, `jpeg`, `png` et `heic`.

### Vidéos — QuickTime `mvhd`

| Extension | Date lue dans le conteneur ? |
| --- | --- |
| `.mp4`, `.mov`, `.m4v`, `.3gp` | **oui** — `creation_time` de la boîte `mvhd` (UTC, époque Mac 1904, converti en heure locale) |
| `.avi`, `.mkv`, `.wmv`, `.flv`, `.webm`, `.ts`, `.mts`, `.m2ts` | non — ces conteneurs n'ont pas de `mvhd` : nom de fichier, puis mtime |

Le lecteur ne parcourt que `moov → mvhd` et ignore le reste : les parseurs d'arbre complet
échouent silencieusement sur les boîtes propres à Apple (`tapt`, `clef`, `prof`, `enof`). Un
`creation_time` à 0 est rejeté (vignettes `SYNOPHOTO_FILM_M.mp4` de DSM).

---

## Supervision

`GET /healthz` (JSON, **503 si plus aucun progrès** — typiquement un montage NFS figé) et
`GET /livez` (200 tant que le processus répond). Format et règles :
[docs/HEALTHZ.md](docs/HEALTHZ.md).

## Développement

```bash
git config core.hooksPath .githooks      # fmt + clippy + tests avant chaque commit
cargo test                               # unitaires + intégration
scripts/smoke-test.sh IMAGE              # bout en bout, en conteneur
```

Prérequis : Rust stable (`rustup`), Docker, `shellcheck`. Plus de chaîne de
cross-compilation : l'image se compile nativement sur un runner arm64.

## Contribuer

- Commits en anglais, [Conventional Commits](.github/COMMIT_CONVENTION.md), trunk-based
  (pas de PR).
- Chaque commit doit passer `cargo fmt --check`, `cargo clippy -D warnings` et les tests
  unitaires (imposé par le hook `pre-commit`).
- Pas de fixture binaire, sauf extrait de métadonnées documenté dans
  [tests/fixtures/README.md](tests/fixtures/README.md) ; sinon générer au runtime avec
  `tests/common/`.

## Licence

MIT — voir [LICENSE](LICENSE).

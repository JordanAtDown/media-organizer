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
- **Transactionnel** : tout est traité dans une zone de travail privée, puis publié d'un
  coup dans la librairie. Une coupure en cours de route se rattrape au scan suivant
  (journal de reprise).
- Conflits de nom entre fichiers différents : `rename`, `skip` ou `overwrite`.
- Logs JSON sur stdout, arrêt propre sur SIGTERM, `/healthz` pour la supervision.

## Architecture

```text
toutes les poll_interval_secs (30 s)
   │
   1 INVENTAIRE      parcours, élagage des dossiers parasites, filtres
   2 PLANIFICATION   date (EXIF → QuickTime → nom → mtime) + destination      lecture seule
   3 RÉSOLUTION      doublons écartés, noms finaux alloués                     en mémoire
   4 PRÉPARATION     copie dans la zone de travail privée (+ EXIF)             zone de travail seule
   5 COMMIT          copie sous un nom caché dans la librairie, puis           rafale rapide
                     renommage atomique de tous les fichiers
```

**La librairie n'est pas touchée avant la phase 5**, et un fichier n'y apparaît sous son vrai
nom qu'une fois complet. Détails et invariants : [docs/PIPELINE.md](docs/PIPELINE.md).

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

### Les trois catégories de dossiers

Le service distingue trois rôles, et le compose les déclare **un par un** — pas de méga-montage :

| Catégorie | Rôle | Dans le conteneur |
| --- | --- | --- |
| **inputs** | dossiers **sources** : ceux qui servent à la synchronisation | un volume par dossier, sous `/inputs/<nom>` |
| **libraries** | dossiers qui **accueillent les librairies** (destination finale) | un volume par dossier, sous `/libraries/<nom>` |
| **work** | zone de travail du service (transit, journal) | interne, dans `state/` — à monter seulement pour déboguer |

`config.toml` relie un input à une library **par leur nom**, sans réécrire de chemin :

```toml
[[folders]]
input   = "jordan"      # → /inputs/jordan
library = "jordan"      # → /libraries/jordan

[[folders]]
input      = "sony"
library    = "sony"
extensions = ["arw"]
```

Un nom sans `/` est cherché sous `/inputs` ou `/libraries` (réglable avec `inputs_root` et
`libraries_root`) ; un chemin absolu reste possible.

### Exemple de `docker-compose.yml`

Trois éléments côte à côte sur le Pi : le compose, `config.toml` (copié de
[config.example.toml](config.example.toml)) et un dossier `state/`.

```yaml
# Exemple de déploiement. Épinglez toujours une version : il n'existe pas de tag `latest`.
#
# Trois catégories de dossiers, chacune montée séparément :
#   /inputs/<nom>     les dossiers SOURCES (synchronisation)
#   /libraries/<nom>  les dossiers qui ACCUEILLENT les librairies (destination finale)
#   la zone de travail (transit, journal) est interne : elle vit dans ./state, rien à monter.
# config.toml relie un input à une library par leur NOM (voir config.example.toml).
services:
  media-organizer:
    image: ghcr.io/jordanatdown/media-organizer:v1.1.0
    container_name: media-organizer
    restart: unless-stopped
    environment:
      # Uid/gid sous lesquels tourne le processus : c'est l'identité vue par le NAS
      # (l'export NFS est en all_squash).
      PUID: "1000"
      PGID: "1000"
      # Obligatoire : sans lui, les dates sont en UTC et les fichiers changent de dossier.
      TZ: "Europe/Paris"
      # Facultatif — tout réglage a un équivalent MO_* (voir le README) :
      # MO_LOG_LEVEL: "debug"
    volumes:
      # Configuration, en lecture seule.
      - ./config.toml:/etc/media-organizer/config.toml:ro
      # Caches persistants (doublons, empreintes, fichiers sans date) et zone de travail.
      - ./state:/var/lib/media-organizer

      # ── inputs : un volume par dossier de synchronisation ──────────────────────────
      - /mnt/nas/nextcloud/jordan/files/synchronisation:/inputs/jordan
      - /mnt/nas/nextcloud/sylvie/files/synchronisation:/inputs/sylvie
      - /mnt/nas/drive/Synchronisation/Sony Alpha 7:/inputs/sony

      # ── libraries : un volume par librairie ────────────────────────────────────────
      - /mnt/nas/nextcloud/jordan/files/photos/phototeque:/libraries/jordan
      - /mnt/nas/nextcloud/sylvie/files/photos/phototeque:/libraries/sylvie
      - /mnt/nas/homes/jordan/Sony A7II:/libraries/sony

      # Pour déboguer la zone de travail, la rendre visible (facultatif) :
      # - ./work:/var/lib/media-organizer/work
    ports:
      # /healthz et /livez, limités à la machine (Gatus, healthcheck local…).
      - "127.0.0.1:8080:8080"
    # Le HEALTHCHECK est déjà défini dans l'image (`media-organizer healthcheck`).
```

Pas à pas :

1. Dans le compose, montez **chaque** dossier source sous `/inputs/<nom>` et **chaque**
   librairie sous `/libraries/<nom>` ; le `<nom>` est celui écrit dans `config.toml`.
2. `PUID` / `PGID` : l'uid et le gid qui ont les droits sur le NAS. Seul `state/` leur est
   attribué automatiquement ; les volumes média ne sont **jamais** modifiés (pas de `chown`
   récursif sur le NAS).
3. Validez, puis essayez en simulation avant le premier vrai passage :

   ```bash
   docker compose run --rm media-organizer validate-config
   docker compose run --rm media-organizer --dry-run --once
   ```

4. Démarrez : `docker compose up -d`. Suivez les logs avec `docker compose logs -f` et l'état
   avec `curl http://127.0.0.1:8080/healthz`.
5. Pour changer de version : modifiez le tag dans `image:` puis `docker compose pull &&
   docker compose up -d`.

**Ce que coûtent des montages séparés.** Un lien physique est impossible entre deux
montages : chaque fichier est donc **copié** (lu sur l'input, écrit sur la librairie, en
passant par le Pi) au lieu d'être lié. Pour des photos qui arrivent au fil de l'eau c'est
négligeable ; un premier passage sur un gros arriéré sera simplement plus long
(`[staging] max_batch` le découpe en lots).

Publier une version : [RELEASING.md](RELEASING.md). Contrat complet de déploiement (volumes,
variables, supervision, première mise en service) : [docs/DEPLOYMENT.md](docs/DEPLOYMENT.md).

---

## Prérequis

| Élément | Valeur |
| --- | --- |
| Machine | Raspberry Pi 5 (**image `linux/arm64` uniquement**) |
| Stockage média | NAS monté en **NFS** ; chaque input et chaque librairie peut être un montage distinct |
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
- La **zone de travail** est privée au service (`<state_dir>/work`) : inutile de la monter.
- Chaque `[[folders]]` a un nom **unique** (par défaut, celui de l'input) : c'est aussi le nom
  de son sous-dossier de travail.

```toml
poll_interval_secs = 30

[[folders]]
input        = "jordan"      # → /inputs/jordan
library      = "jordan"      # → /libraries/jordan
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

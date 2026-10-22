---
name: verify-container
description: Build and run the media-organizer container to prove it works — locally on amd64, under arm64 emulation, or against a published ghcr tag. Use whenever the user wants to test the image, run the smoke test, check the arm64 build, or confirm a published version actually runs, and before declaring any Dockerfile or entrypoint change done.
---

# Vérifier l'image conteneur

L'image est **arm64 uniquement** (Raspberry Pi 5) et la machine de développement est
amd64 : sans émulation, on ne peut pas exécuter ce qui sera réellement publié. Ce skill
couvre les trois niveaux de preuve, du moins au plus fiable.

## 1 — Build natif local (rapide, amd64)

Valide le Dockerfile, l'entrypoint et le comportement de l'application, pas l'architecture.

```bash
docker build -t media-organizer:test .
scripts/smoke-test.sh media-organizer:test
```

Le smoke test vérifie : `--dry-run` n'écrit rien ; le rangement par EXIF/nom/mtime ; le
`warn` du repli mtime ; `@eaDir` élagué ; modes `666`/`777` sur le fichier **et** chaque
répertoire ; EXIF injecté sans réécrire la source ; transit vide ; doublon laissé en place
et mémorisé sans `_1` ; `/healthz` ok et `/livez` 200 ; `docker stop` en ≤ 3 s avec **code
de sortie 0** (un `143` signifierait un SIGTERM non géré).

## 2 — Émulation arm64 (avant une release, une fois)

Rien n'est enregistré par défaut (`/proc/sys/fs/binfmt_misc/` ne contient que
`WSLInterop`…) et le builder `docker` ne fait pas de multi-plateforme.

```bash
docker run --privileged --rm tonistiigi/binfmt --install arm64   # modifie l'hôte : demander
docker buildx create --use --name mo-arm64 --driver docker-container
docker buildx build --platform linux/arm64 --load -t media-organizer:arm64 .   # 10-20 min sous QEMU
docker run --rm --platform linux/arm64 media-organizer:arm64 --version
DOCKER_PLATFORM=linux/arm64 scripts/smoke-test.sh media-organizer:arm64
```

À faire une seule fois pour écarter un bug propre à l'architecture : le parseur ISOBMFF
utilise des `from_be_bytes` explicites donc il *devrait* passer, et c'est ce que les tests
vérifient.

## 3 — L'artefact publié (le seul qui compte)

Après une release, contre le tag **réellement publié** (build natif arm64 par la CI) :

```bash
docker logout ghcr.io                     # prouve en même temps que le paquet est public
docker buildx imagetools inspect ghcr.io/jordanatdown/media-organizer:vX.Y.Z
docker run --rm --platform linux/arm64 ghcr.io/jordanatdown/media-organizer:vX.Y.Z --version
DOCKER_PLATFORM=linux/arm64 scripts/smoke-test.sh ghcr.io/jordanatdown/media-organizer:vX.Y.Z
```

La preuve finale est un `docker run --dry-run --once` sur le Pi lui-même contre un couple
entrée/sortie jetable sous `/mnt/nas-nextcloud/` — hors périmètre de ce dépôt, mais la
commande exacte est dans `docs/DEPLOYMENT.md`.

## Règles

- Les actions qui modifient l'hôte (binfmt, builder) se **demandent** d'abord.
- Ne jamais déclarer un changement de Dockerfile/entrypoint terminé sans avoir exécuté le
  smoke test, au moins au niveau 1.

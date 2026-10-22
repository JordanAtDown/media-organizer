---
name: release
description: Cut and verify a media-organizer release — CHANGELOG section, scripts/release.sh, push the tag, then prove the arm64 image is published and pullable anonymously. Use whenever the user asks to release, publish, tag, ship a version, or bump the version, or asks whether a version is available on ghcr.
---

# Publier une version de media-organizer

Une version = un tag `vX.Y.Z` = une image `ghcr.io/jordanatdown/media-organizer:vX.Y.Z`
(arm64) + une release GitHub. Procédure détaillée : [RELEASING.md](../../../RELEASING.md).

**Pousser le tag est ce qui publie.** Ne jamais pousser sans demande explicite de
l'utilisateur, même si le script a tout préparé : `scripts/release.sh` pose le tag en local
et *affiche* la commande, il ne la lance pas.

## Cycle

1. **Choisir la version** (semver) à partir de `git log` depuis le dernier tag :
   `feat` → mineure, `fix` seul → correctif, `!`/`BREAKING CHANGE` → majeure.
2. **CHANGELOG** : déplacer `[Unreleased]` sous `## [X.Y.Z] - AAAA-MM-JJ`. Le workflow
   échoue si cette section manque.
3. **Contrôles** : `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`,
   `cargo test`, `cargo audit`. Vérifier que `README.md`, `config.example.toml` et
   `docs/DEPLOYMENT.md` reflètent le code.
4. `bash scripts/release.sh X.Y.Z` (depuis `main`, arbre propre).
5. **Demander confirmation**, puis `git push origin main vX.Y.Z`.
6. Suivre le workflow : `gh run list --workflow release.yml`, `gh run view <id>`.
7. **Vérifier l'artefact publié** (jamais « le workflow est vert » seul) :

   ```bash
   docker logout ghcr.io
   docker buildx imagetools inspect ghcr.io/jordanatdown/media-organizer:vX.Y.Z
   ```

   Voir le skill `verify-container` pour exécuter l'image.

## Pièges

- **Pas de tag `latest`.** L'infrastructure épingle une version exacte ; ne pas en ajouter.
- Un paquet GHCR naît **privé** même depuis un dépôt public. Si l'étape « tirage anonyme »
  du workflow échoue sur un 401, il faut le passer en *Public* (une seule fois) :
  GitHub → *Packages* → `media-organizer` → *Package settings* → *Change visibility*.
- Les runners arm64 gratuits exigent un **dépôt public**.
- Si le tag est mauvais : ne pas le déplacer en silence sur un tag déjà poussé ; en parler à
  l'utilisateur, une version publiée est immuable pour l'infrastructure.

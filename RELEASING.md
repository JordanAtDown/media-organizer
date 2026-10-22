# Publier une version

Une version = un tag `vX.Y.Z` = une image `ghcr.io/jordanatdown/media-organizer:vX.Y.Z`
(arm64) + une release GitHub. **Il n'existe pas de tag `latest`** : l'infrastructure épingle
toujours une version exacte, en publier un flottant n'est qu'une surface d'erreur.

## Cycle

1. Dans [CHANGELOG.md](CHANGELOG.md), déplacer les notes de `[Unreleased]` sous un nouveau
   `## [X.Y.Z] - AAAA-MM-JJ`. Le workflow **échoue** si cette section manque.
2. Vérifier que `README.md`, `config.example.toml` et `docs/DEPLOYMENT.md` reflètent le code.
3. Depuis `main`, arbre propre :

   ```bash
   bash scripts/release.sh X.Y.Z
   ```

   Il met à jour `Cargo.toml` et `Cargo.lock`, vérifie que tout compile, commit
   `chore(release): bump version to X.Y.Z` et pose le tag `vX.Y.Z`. **Il ne pousse rien** :
   il affiche la commande.
4. Pousser — seulement quand vous êtes prêt, c'est ce qui publie :

   ```bash
   git push origin main vX.Y.Z
   ```

5. Le workflow [release.yml](.github/workflows/release.yml) compile l'image **nativement
   sur un runner arm64**, la pousse, puis fait `docker logout` et `imagetools inspect` pour
   prouver qu'elle se tire **anonymement**, et crée la release GitHub.
6. Vérifier le résultat publié (voir ci-dessous).

## Deux gestes manuels, une seule fois

- **Dépôt public** : les runners arm64 gratuits (`ubuntu-24.04-arm`) ne sont disponibles
  que pour les dépôts publics, et l'image doit être tirable sans secret de registre.

  ```bash
  gh repo edit JordanAtDown/media-organizer --visibility public
  ```

- **Paquet GHCR public** : un paquet GHCR peut naître **privé**, même depuis un dépôt
  public. Pour ce dépôt il a hérité de la visibilité publique dès la première publication
  (`v1.0.0`), mais ne le supposez pas : l'étape « tirage anonyme » du workflow le vérifie à
  chaque version. Si elle échoue sur un `401`, passez le paquet en *Public* : GitHub →
  profil → *Packages* → `media-organizer` → *Package settings* → *Change visibility*.
  Mieux vaut un build rouge qu'un `401` silencieux sur le Pi. Le label
  `org.opencontainers.image.source` de l'image relie le paquet au dépôt.

## Vérifier une version publiée

```bash
docker logout ghcr.io                                         # prouve l'anonymat
docker buildx imagetools inspect ghcr.io/jordanatdown/media-organizer:vX.Y.Z
docker run --rm --platform linux/arm64 \
  ghcr.io/jordanatdown/media-organizer:vX.Y.Z --version
DOCKER_PLATFORM=linux/arm64 scripts/smoke-test.sh \
  ghcr.io/jordanatdown/media-organizer:vX.Y.Z
```

Sur une machine amd64 sans émulation arm64, ces deux dernières commandes exigent d'abord
`docker run --privileged --rm tonistiigi/binfmt --install arm64`. La preuve définitive
reste un `docker run` sur le Pi lui-même, en `--dry-run`.

## Contrôles de sécurité des dépendances

```bash
cargo audit          # advisories acceptées et justifiées : .cargo/audit.toml
cargo outdated
```

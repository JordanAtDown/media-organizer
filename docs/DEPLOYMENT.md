# Contrat de déploiement

Ce que doit savoir quiconque déploie l'image — en particulier la future tâche Ansible du
dépôt d'infrastructure. Ce dépôt-ci ne déploie rien lui-même.

## Image

- `ghcr.io/jordanatdown/media-organizer:vX.Y.Z`, **publique**, tirable sans authentification.
- **`linux/arm64` uniquement** (Raspberry Pi 5). Pas de tag `latest`, pas de tag flottant :
  épingler la version exacte.
- Binaire = `PID 1` (via `exec`), logs sur **stdout** (JSON par défaut), arrêt propre sur
  SIGTERM (code de sortie `0`, en moins d'une seconde même pendant l'attente entre scans).

## Variables d'environnement

| Variable | Obligatoire | Rôle |
| --- | --- | --- |
| `TZ` | **oui** | fuseau horaire. Sans lui les dates sont en UTC et les fichiers changent de dossier au passage de mois. Le fuseau détecté est journalisé au démarrage (`timezone_offset`) |
| `PUID` / `PGID` | recommandé (défaut 1000) | uid/gid sous lesquels le processus tourne : **c'est l'identité vue par le NAS** |
| `MO_CONFIG` | non | défaut `/etc/media-organizer/config.toml` |
| `MO_STATE_DIR` | non | défaut `/var/lib/media-organizer` |
| `MO_LOG_FORMAT`, `MO_LOG_LEVEL`, `MO_DRY_RUN`, `MO_ONCE`, `MO_POLL_INTERVAL_SECS`, `MO_WORKERS`, `MO_HEALTH_BIND` | non | voir le [README](../README.md) |

## Volumes

Trois catégories de dossiers, déclarées **une par une** (voir le README) :

| Montage | Mode | Contenu |
| --- | --- | --- |
| `/etc/media-organizer/config.toml` | `ro` | configuration (templatable : clés inconnues = erreur) |
| `/var/lib/media-organizer` | `rw` | caches persistants **et zone de travail** (`work/`). Volume distinct de la configuration |
| `/inputs/<nom>` | `rw` | un dossier **source** (synchronisation), un volume par dossier |
| `/libraries/<nom>` | `rw` | un dossier qui **accueille une librairie**, un volume par dossier |

`config.toml` relie un input à une library par leur nom (`input = "jordan"`,
`library = "jordan"`). Les racines `/inputs` et `/libraries` se règlent avec `inputs_root` et
`libraries_root` ; des chemins absolus restent possibles.

**Montages séparés = copie, pas lien physique.** Un `hard_link` est impossible entre deux
montages (`EXDEV`), même s'ils viennent du même export NFS : chaque fichier est copié (lu sur
l'input, écrit sur la librairie, via le Pi). Ce n'est pas un problème pour un flux normal ;
un gros arriéré au premier passage est plus long, borné par `[staging] max_batch`.

**La librairie ne reçoit que le résultat.** Tout le travail se fait dans la zone de travail
privée ; la librairie reçoit des copies sous un nom caché `.<nom>.mo-part` (que les scanners
de librairie comme Immich n'indexent pas : ils ne retiennent que les noms qui *finissent* par
une extension média), puis les fichiers prennent leur vrai nom d'un coup, par un lien
atomique qui n'écrase jamais un fichier existant. Après une coupure franche, d'éventuels
`.mo-part` orphelins sont repris ou purgés au scan suivant.

La zone de travail n'a pas à être montée. Pour la déboguer, montez `./work` sur
`/var/lib/media-organizer/work`.

L'entrypoint ne fait un `chown` **que** sur le dossier d'état — jamais sur les volumes
média (ce serait une réécriture récursive de tout le NAS via NFS). Les uid/gid `PUID`/`PGID`
doivent avoir les droits d'écriture sur chaque librairie et d'écriture/suppression sur chaque
input (la source est supprimée après publication).

## Réseau et supervision

Port `8080` (`health.bind`). Voir [HEALTHZ.md](HEALTHZ.md) pour le détail.

- Probe de supervision (ex. Gatus) : `GET http://<hôte>:8080/healthz`, condition sur le
  code HTTP (`200` = vivant, `503` = bloqué). Pour aussi alerter sur `degraded`, comparer
  `status` à `ok` dans le corps.
- Healthcheck Docker intégré : `media-organizer healthcheck` (intervalle 30 s, démarrage
  toléré 60 s).
- Le conteneur monte des bind-mounts NFS : il a sa place dans la liste des conteneurs
  qu'un watchdog doit relancer quand le montage revient.

## Première mise en service

1. Valider la configuration sans rien démarrer :

   ```bash
   docker run --rm -v ./config.toml:/etc/media-organizer/config.toml:ro \
     ghcr.io/jordanatdown/media-organizer:vX.Y.Z validate-config
   ```

2. **Essayer sur un couple entrée/sortie jetable**, en `--dry-run` d'abord : il exécute
   l'inventaire, la planification et la résolution complets et affiche, pour chaque
   fichier, la **source de sa date** et sa destination. On voit exactement combien de
   fichiers seraient datés par le `mtime` (approximation) et où ils iraient.

   ```bash
   docker run --rm --user 1000:1000 -e TZ=Europe/Paris \
     -v ./config.toml:/etc/media-organizer/config.toml:ro \
     -v ./state:/var/lib/media-organizer -v /mnt/nas-nextcloud:/mnt/nas-nextcloud \
     ghcr.io/jordanatdown/media-organizer:vX.Y.Z --dry-run --once
   ```

3. Seulement ensuite, lancer en démon sur les vrais dossiers.

## Changement de comportement par rapport à la v1.2.4 de `syno-media-organizer`

- Un fichier sans date exploitable est **désormais rangé** d'après son `mtime` au lieu
  de rester dans le dossier de dépôt (`use_mtime_fallback = false` rétablit l'ancien contrat).
  Il peut donc atterrir dans un mois inexact — d'où le `warn` et le compteur dédié.
- Un doublon exact n'est plus renommé `_1` : il reste dans `input` et est mémorisé.
- Le cache `no_date_cache.json` du NAS contient des chemins `/volume1/…` qui n'existent
  plus vus du Pi : **on repart d'un cache vide**. Le premier scan journalise une fois les
  fichiers sans date, puis se tait.

## Ce qui disparaît côté NAS une fois migré

Le paquet DSM, l'utilisateur technique et ses ACL Synology sur les dossiers Nextcloud : le
Pi accède au NFS avec son propre uid. (À traiter dans le dépôt d'infrastructure, pas ici.)

## Limite connue : pas d'image amd64

L'image est volontairement arm64 seulement. Elle **ne peut pas tourner sur une VM de
staging x86_64**. Si l'infrastructure impose « staging avant prod », il faudra soit valider
directement sur le Pi contre un couple entrée/sortie jetable (en `--dry-run` d'abord, ce
qui est sûr), soit réintroduire une image amd64 pour le seul staging. C'est un arbitrage
à trancher au moment de la tâche de déploiement.

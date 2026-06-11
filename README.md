# Quick

Une implémentation Rust/Pingora du noyau de
[Shopify Quick](https://shopify.engineering/quick): déposer un dossier HTML et
obtenir immédiatement un site adressable par sous-domaine.

## Fonctionnalités

- remplacement transactionnel d'un dossier statique avec restauration sur erreur;
- homepage de déploiement avec sélection ou drag-and-drop d'un dossier;
- upload d'archives ZIP avec extraction sécurisée;
- homepage entièrement embarquée dans le binaire, sans asset externe;
- configuration du hostname et retour immédiat de l'URL publiée;
- routage `<site>.<domaine>` avec Pingora;
- fichiers `index.html` pour les répertoires;
- fallback `index.html` pour les applications monopage;
- détection MIME;
- refus des liens symboliques et des traversées de chemin.
- stockage local ou S3 compatible avec publications atomiques.

Quick n'implémente pas encore les API de base de données, fichiers, IA,
entrepôt de données, WebSockets ou identité.

## Démarrage

```sh
cargo build
cargo run -- serve
```

Ouvrir <http://localhost:8080>, choisir un dossier contenant un `index.html`,
puis définir son hostname. Le site sera disponible sur
`http://<hostname>.localhost:8080`.

La homepage accepte aussi une archive ZIP. Si tous ses fichiers se trouvent
dans un même dossier racine, ce dossier est retiré automatiquement. Les
archives sont limitées à 1 000 entrées et 25 Mo décompressés; les chemins
sortants, liens symboliques et fichiers chiffrés sont refusés.

Le déploiement en ligne de commande reste disponible:

```sh
cargo run -- deploy examples/hello --site hello
```

Ouvrir ensuite <http://hello.localhost:8080>. Les navigateurs modernes
résolvent généralement `*.localhost` vers `127.0.0.1`. Sinon:

```sh
curl -H 'Host: hello.localhost' http://127.0.0.1:8080/
```

Options utiles:

```sh
quick serve --listen 0.0.0.0:8080 --sites-dir ./sites --base-domain quick.internal
quick deploy ./dist --site mon-site --sites-dir ./sites
```

## Stockage S3

Quick peut stocker les sites dans AWS S3 ou un service compatible comme MinIO,
Cloudflare R2 ou Backblaze B2. Chaque déploiement est écrit sous un préfixe de
release immuable, puis publié en remplaçant `current.json`.

Les identifiants suivent la chaîne standard AWS (`AWS_ACCESS_KEY_ID`,
`AWS_SECRET_ACCESS_KEY`, `AWS_SESSION_TOKEN`, profils et rôles d'instance).

AWS S3:

```sh
quick serve \
  --storage s3 \
  --s3-bucket quick-sites \
  --s3-region eu-west-1
```

MinIO ou autre endpoint compatible:

```sh
AWS_ACCESS_KEY_ID=minio \
AWS_SECRET_ACCESS_KEY=minio-secret \
quick serve \
  --storage s3 \
  --s3-bucket quick-sites \
  --s3-region us-east-1 \
  --s3-endpoint http://127.0.0.1:9000 \
  --s3-path-style
```

Les options ont aussi des variables `QUICK_STORAGE`, `QUICK_S3_BUCKET`,
`QUICK_S3_REGION`, `QUICK_S3_ENDPOINT`, `QUICK_S3_PREFIX` et
`QUICK_S3_PATH_STYLE`. Le bucket doit exister avant le démarrage.

Politique IAM minimale pour le préfixe par défaut:

```json
{
  "Version": "2012-10-17",
  "Statement": [{
    "Effect": "Allow",
    "Action": ["s3:GetObject", "s3:PutObject"],
    "Resource": "arn:aws:s3:::quick-sites/quick/*"
  }]
}
```

Les anciennes releases restent immuables dans le bucket. Configurer une règle
de cycle de vie S3 sur `quick/sites/*/releases/` pour les supprimer selon la
durée de rétention souhaitée.

En production, placer le service derrière un proxy d'identité (IAP, oauth2-proxy
ou équivalent), comme dans l'architecture Shopify. Quick sert volontairement
HTTP sans authentification ni TLS. La homepage permet donc à toute personne qui
peut joindre le serveur de créer ou remplacer un site.

## Architecture

```text
quick deploy ./dist --site demo
             |
             v
 local: sites/demo/*
   ou
 S3: quick/sites/demo/releases/<id>/*
             |
             v
demo.localhost -> Pingora -> fichier statique
```

## Tests

```sh
cargo test
```

## Releases

Les tags sémantiques `vX.Y.Z` déclenchent le workflow GitHub Actions de release.
La version du tag doit correspondre à celle de `Cargo.toml`.

```sh
git tag v0.1.0
git push origin v0.1.0
```

Après les vérifications (`fmt`, Clippy et tests), GitHub publie une release avec:

- Linux x86_64;
- macOS Intel;
- macOS Apple Silicon;
- une somme SHA-256 pour chaque archive.

Le workflow peut aussi être lancé manuellement depuis GitHub Actions; dans ce
cas, il compile et conserve les artefacts sans créer de GitHub Release.

Chaque build vérifie que la homepage est présente dans le binaire final. Le job
Linux lance également une copie isolée de l'exécutable depuis un dossier vide
et contrôle que la page d'administration répond sans fichier annexe.

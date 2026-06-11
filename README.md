# Quick MVP

Une implémentation Rust/Pingora du noyau de
[Shopify Quick](https://shopify.engineering/quick): déposer un dossier HTML et
obtenir immédiatement un site adressable par sous-domaine.

## Fonctionnalités du MVP

- remplacement transactionnel d'un dossier statique avec restauration sur erreur;
- homepage de déploiement avec sélection ou drag-and-drop d'un dossier;
- configuration du hostname et retour immédiat de l'URL publiée;
- routage `<site>.<domaine>` avec Pingora;
- fichiers `index.html` pour les répertoires;
- fallback `index.html` pour les applications monopage;
- détection MIME;
- refus des liens symboliques et des traversées de chemin.

Ce MVP n'implémente pas encore les API Quick de base de données, fichiers, IA,
entrepôt de données, WebSockets ou identité.

## Démarrage

```sh
cargo build
cargo run -- serve
```

Ouvrir <http://localhost:8080>, choisir un dossier contenant un `index.html`,
puis définir son hostname. Le site sera disponible sur
`http://<hostname>.localhost:8080`.

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

En production, placer le service derrière un proxy d'identité (IAP, oauth2-proxy
ou équivalent), comme dans l'architecture Shopify. Le MVP sert volontairement
HTTP sans authentification ni TLS. La homepage permet donc à toute personne qui
peut joindre le serveur de créer ou remplacer un site.

## Architecture

```text
quick deploy ./dist --site demo
             |
             v
       sites/demo/*
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

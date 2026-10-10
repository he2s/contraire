# Compose: one file, many containers

Real applications are a team of containers: a website, a database, maybe a cache. Compose describes the whole team in one file.

## A complete small app

Save this as `compose.yaml`:

```yaml
services:
  web:
    image: nginx:alpine
    ports:
      - "8080:80"
    depends_on:
      - api

  api:
    image: ghcr.io/example/api:1.4
    environment:
      DATABASE_URL: postgres://app:secret@db/app
    depends_on:
      db:
        condition: service_healthy

  db:
    image: postgres:16
    environment:
      POSTGRES_USER: app
      POSTGRES_PASSWORD: secret
    volumes:
      - dbdata:/var/lib/postgresql/data
    healthcheck:
      test: ["CMD", "pg_isready", "-U", "app"]
      interval: 5s

volumes:
  dbdata:
```

Then, in the same folder:

```sh
docker compose up -d      # create and start everything
docker compose ps         # what's running
docker compose logs -f    # follow all logs together
docker compose down       # stop and remove (volumes are kept)
```

## What you get for free

- **A private network** where `web`, `api` and `db` find each other by service name.
- **Start order**: `depends_on` makes `db` start first. With `condition: service_healthy`, `api` even waits until the database's health check passes.
- **One name for the whole thing**, the *project*, taken from the folder name unless you set `-p`.


```live stacks
In the dashboard this lists the Compose stacks on your own host.
```

## How this dashboard understands Compose

Compose labels every container it creates: which project, which service, which file. This dashboard reads those labels, so it can:

- **Group** containers by project, each with its own colour.
- Show **service names** (`web`, `api #2`) instead of generated names like `shop-api-2`.
- **Start, stop or restart a whole project** with the buttons next to its name, following the `depends_on` order: dependencies start first and stop last.
- Show the **compose file and folder** a project came from, with a button that copies the exact `docker compose up` command.

> **Good to know:** the dashboard doesn't need the compose file itself, and it never edits it. Everything it knows comes from the running containers. To *change* a project (new image, new port), edit `compose.yaml` and run `docker compose up -d` again. Compose only recreates what changed.

```quiz
Q: `api` has `depends_on: db` with `condition: service_healthy`. What does Compose do on `up`?
- [ ] Starts both at the same time
- [x] Starts db, waits until its health check passes, then starts api
- [ ] Starts api first and restarts it until db is there
> `depends_on` sets the order; the health condition makes Compose wait for real readiness, not just "started". This dashboard's stack actions follow the same order.
```

Next: [On a Raspberry Pi](raspberry-pi.md).

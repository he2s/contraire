# Docker cheat sheet

The commands you'll actually use, on one page.

## Containers

| Command | What it does |
|---|---|
| `docker ps` | List running containers (`-a` includes stopped ones) |
| `docker run -d --name web -p 8080:80 nginx:alpine` | Start a container in the background |
| `docker stop web` / `docker start web` | Stop / start it again |
| `docker restart web` | Stop, then start |
| `docker rm web` | Delete a stopped container (`-f` stops it first) |
| `docker logs -f --tail 100 web` | Follow the last 100 log lines |
| `docker exec -it web sh` | Open a shell inside a running container |
| `docker stats` | Live CPU and memory for all containers |
| `docker inspect web` | Everything Docker knows about it, as JSON |

## Images

| Command | What it does |
|---|---|
| `docker images` | List images |
| `docker pull nginx:alpine` | Download an image |
| `docker build -t myapp:1.0 .` | Build from `./Dockerfile` |
| `docker rmi nginx:alpine` | Delete an image |
| `docker image prune` | Delete unused, untagged images |

## Compose

| Command | What it does |
|---|---|
| `docker compose up -d` | Create and start the project in this folder |
| `docker compose ps` | List the project's containers |
| `docker compose logs -f api` | Follow one service's logs |
| `docker compose pull && docker compose up -d` | Update to newer images |
| `docker compose down` | Stop and remove (volumes stay) |

## Data and networks

| Command | What it does |
|---|---|
| `docker volume ls` | List volumes |
| `docker network ls` | List networks |
| `docker system df` | How much disk Docker is using |
| `docker system prune` | Remove stopped containers, unused networks and dangling images |

## Exit codes, decoded

| Code | Usually means |
|---|---|
| `0` | Finished normally |
| `1` | The app reported an error; check its logs |
| `125` | Docker couldn't start it (bad option) |
| `126` / `127` | Command not executable / not found |
| `137` | Killed: out of memory, or force-stopped |
| `139` | Crashed (segmentation fault) |
| `143` | Stopped politely (SIGTERM) |

> **Rule of thumb:** when something misbehaves, open it on the **Containers** page and read the **logs** first. The answer is in there nine times out of ten.

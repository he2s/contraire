# Keeping your data

Containers are meant to be disposable. Your data is not. Keep the two apart.

## The rule

Anything written *inside* a container lands in its thin writable layer, and **disappears when the container is removed**. Upgrading an app usually means removing the old container and starting a new one from a newer image, so data must live somewhere else.

## Volumes: storage that Docker manages

A **volume** is a named piece of storage that outlives any container:

```sh
docker volume create pgdata
docker run -d --name db -v pgdata:/var/lib/postgresql/data \
  -e POSTGRES_PASSWORD=secret postgres:16
```

Remove the container, start a new one with the same `-v pgdata:…`, and every row is still there. In the detail panel, mounts show as `pgdata → /var/lib/postgresql/data`.

## Bind mounts: a folder from the host

A **bind mount** maps a folder you choose straight into the container. Handy for configuration files you want to edit yourself:

```sh
docker run -d -p 8080:80 \
  -v "$PWD/site:/usr/share/nginx/html:ro" nginx:alpine
```

The `:ro` suffix makes it **read-only** inside the container: the container can serve the files but not change them.

| | Volume | Bind mount |
|---|---|---|
| Who picks the location | Docker | You |
| Best for | Databases, app state | Config files, source code |
| Survives `docker rm` | Yes | Yes (it's your folder) |
| Removed by | `docker volume rm` | Never by Docker |

> **On a Raspberry Pi:** SD cards wear out from constant writes. Put busy databases and logs on a USB SSD and point your volumes there. Your card will last years longer.

```quiz
Q: You remove a database container and start a new one from the same image. Where is your data?
- [ ] Gone, whatever you did
- [x] Still there, if it lived in a volume or bind mount
- [ ] In the image, which saved it automatically
> The container's own writable layer disappears with it; volumes and bind mounts live on. Mount the same volume into the new container and the data is back.
```

## Backing up a volume

```sh
docker run --rm -v pgdata:/data -v "$PWD:/backup" alpine \
  tar czf /backup/pgdata.tar.gz -C /data .
```

This runs a throwaway container that sees both the volume and your current folder, packs one into the other, and removes itself.

Next: [Compose](compose.md).

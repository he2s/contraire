# On a Raspberry Pi

A Raspberry Pi 3 has 1 GB of memory and four small cores. That's enough for a surprising amount, if you choose carefully.

## Install Docker

```sh
curl -fsSL https://get.docker.com | sh
sudo usermod -aG docker $USER     # use docker without sudo (log out and in again)
```

> **Careful:** members of the `docker` group effectively have root access to the Pi. Only add accounts you'd trust with the root password.

## Pick images that support ARM

Most popular images are published for several CPU types, and Docker picks the right one automatically. If an image only exists for `amd64` (typical PCs), it won't start on a Pi and exits immediately with an *exec format error*. Check an image before relying on it:

```sh
docker manifest inspect nginx:alpine | grep architecture
```

Look for `arm64` (64-bit Raspberry Pi OS) or `arm` (32-bit).


```live host
In the dashboard this shows your host's CPU, memory and what this dashboard itself uses.
```

## Make every megabyte count

- Prefer **`alpine`** or **`slim`** variants of images: often a tenth of the size.
- Give hungry services a memory limit, so one runaway container can't freeze the whole Pi:

```yaml
services:
  app:
    image: ghcr.io/example/app:1.4
    mem_limit: 256m
```

- Set a **restart policy**, so services come back after a power cut:

```sh
docker update --restart unless-stopped <container>
```

- Clean up old images now and then (the **Images** tab has a button for this):

```sh
docker image prune
```

## Watch the card

SD cards are slow and wear out with constant writing. Keep logs small and put databases on a USB SSD:

```json
{ "log-driver": "local", "log-opts": { "max-size": "10m" } }
```

Save that as `/etc/docker/daemon.json` and restart Docker with `sudo systemctl restart docker`. Container logs then rotate instead of growing forever.

```quiz
Q: An image starts on your PC but exits immediately on the Pi with "exec format error". Why?
- [ ] The Pi has too little memory
- [x] The image was only built for x86 PCs, not ARM
- [ ] Docker is too old on the Pi
> Images are built per CPU architecture. Check with `docker manifest inspect <image>` for `arm64` or `arm`, or pick an image that publishes them.
```

Next: [Cheat sheet](cheat-sheet.md).

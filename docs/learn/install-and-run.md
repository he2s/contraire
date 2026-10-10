# Install and run

One small file, about 800 KB, with no dependencies. On the machine that runs Docker it's the dashboard. On your laptop it's the remote control.

## Raspberry Pi

Works on a Pi 3, 4 and 5, with 32- or 64-bit Raspberry Pi OS. Docker must already be installed (see [On a Raspberry Pi](raspberry-pi.md)).

**1. Install** (the script picks the right build and checks it):

```sh
curl -fsSL https://raw.githubusercontent.com/he2s/contraire/main/install.sh | sudo CONTRAIRE_BIN_DIR=/usr/local/bin sh
```

**2. Choose a password**, because anyone who reaches the dashboard can control Docker:

```sh
sudo mkdir -p /etc/contraire
read -rsp "Dashboard password: " pw && echo "$pw" | sudo tee /etc/contraire/password >/dev/null; unset pw
sudo chown "$USER" /etc/contraire/password && sudo chmod 600 /etc/contraire/password
```

**3. Start it at boot**, as a service that restarts if anything goes wrong:

```sh
sudo tee /etc/systemd/system/contraire.service >/dev/null <<EOF
[Unit]
Description=contraire Docker dashboard
After=docker.service
Requires=docker.service

[Service]
User=$USER
Group=docker
ExecStart=/usr/local/bin/contraire --password-file /etc/contraire/password
StateDirectory=contraire
Restart=on-failure

[Install]
WantedBy=multi-user.target
EOF
sudo systemctl enable --now contraire
```

**4. Open it** from any device on your network: `http://<pi-address>:8083`. Find the address with `hostname -I`. The user name is `admin`.

> The audit log and API tokens are kept in `/var/lib/contraire` (systemd's state directory) and survive updates. Check on the service with `systemctl status contraire`; follow its output with `journalctl -u contraire -f`.

## Linux PC or server

The same as the Raspberry Pi, on any 64-bit Intel/AMD machine. To try it quickly first, without a service:

```sh
curl -fsSL https://raw.githubusercontent.com/he2s/contraire/main/install.sh | sh
contraire -l 127.0.0.1:8083          # only reachable from this machine; open http://127.0.0.1:8083
```

If you get *permission denied* on the Docker socket, your user isn't in the `docker` group yet:

```sh
sudo usermod -aG docker "$USER"     # then log out and back in
```

When you're happy with it, follow steps 2–4 of the Raspberry Pi section.

## Windows

On Windows, contraire is mostly the **remote control** for a dashboard running elsewhere, such as your Pi.

**1. Install** in PowerShell. This also adds contraire to your PATH:

```powershell
irm https://raw.githubusercontent.com/he2s/contraire/main/install.ps1 | iex
```

From the classic command prompt (`cmd.exe`) instead:

```cmd
curl.exe -fsSLO https://raw.githubusercontent.com/he2s/contraire/main/install.cmd && install.cmd
```

**2. Connect** in a new terminal, with a token from the dashboard's **Access** page:

```powershell
$env:CONTRAIRE_HOST = "http://pi.local:8083"
$env:CONTRAIRE_TOKEN = "ctr_..."
contraire ps
```

> To *run* the dashboard on Windows, start Docker Desktop and run `contraire` in a terminal. It talks to Docker through the `\\.\pipe\docker_engine` pipe and serves on port 8083 like everywhere else.

## As a Docker container

If you'd rather not install anything on the host itself:

```sh
docker run -d --name contraire --restart unless-stopped -p 8083:8083 \
  -v /var/run/docker.sock:/var/run/docker.sock -v contraire-state:/state \
  -e CONTRAIRE_AUDIT_FILE=/state/audit.jsonl -e CONTRAIRE_PASSWORD=change-me \
  contraire
```

To see per-container CPU and memory from inside a container, also add `--cgroupns=host --pid=host -v /sys/fs/cgroup:/sys/fs/cgroup:ro`. Without them you only get host metrics. The image is built from the repository (`docker build -t contraire .`, about 1 MB). On Fedora and other SELinux systems, add `--security-opt label=disable`.

## Your laptop as the remote control

Install it the same way (Linux or Windows above), create a token on the dashboard's **Access** page, and then:

```sh
export CONTRAIRE_HOST=http://pi.local:8083 CONTRAIRE_TOKEN=ctr_...
contraire ps                         # all containers
contraire init ops-pi                # a ready-made folder for Claude Code: skill, log, journal
```

Everything else is on the [cheat sheet](cli-cheatsheet.md).

## Update or remove

```sh
curl -fsSL https://raw.githubusercontent.com/he2s/contraire/main/install.sh | sudo CONTRAIRE_BIN_DIR=/usr/local/bin sh && sudo systemctl restart contraire   # update
sudo systemctl disable --now contraire && sudo rm /etc/systemd/system/contraire.service /usr/local/bin/contraire   # remove
```

Your containers are never touched by installing, updating or removing the dashboard.

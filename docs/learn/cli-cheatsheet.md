# Cheat sheet: command line and Claude Code

Everything the dashboard does, from a terminal on your own computer or through an AI agent. Same binary, same permissions, same audit log.

## Set up once

Download the binary for your computer, then tell it where the server is and which token to use. Create the token on the **Access** page.

```sh
export CONTRAIRE_HOST=http://pi.local:8083           # where the dashboard runs
export CONTRAIRE_TOKEN=ctr_...                         # from the Access page, shown once
contraire info                                         # check: host, Docker version, server footprint
```

> Put both `export` lines in `~/.bashrc` or `~/.zshrc` so every new terminal has them.

## Look around

```sh
contraire ps                                           # every container, status in plain words
contraire inspect web                                  # health, exit code explained, ports, mounts, compose deps
contraire stacks                                       # compose stacks and which services need attention
contraire events                                       # starts, stops, crashes and health changes, last hour
contraire events --since 15                            # … only the last 15 minutes
contraire ps --json                                    # raw JSON, for scripts and jq
```

## Read the logs

```sh
contraire logs web                                     # last 100 lines
contraire logs web -n 500                              # last 500 lines
contraire logs web -f                                  # follow live until you press Ctrl+C
contraire logs web -t                                  # with timestamps
contraire logs web -n 1000 | grep -i error             # only the lines that mention errors
```

## Take action

Needs an **operate** or **admin** token. Every action is recorded under the token's name.

```sh
contraire restart web                                  # stop, then start again
contraire stop web                                     # stop; the service is down until started
contraire start web                                    # start a stopped container
contraire pause web                                    # freeze it in place (unpause to continue)
contraire stack shop restart                           # a whole compose stack, in dependency order
contraire stack shop stop                              # dependencies stop last
```

## Claude Code and other agents

Register the built-in MCP server once. Use an **operate** token: the agent can restart things, but never delete them.

```sh
claude mcp add contraire --env CONTRAIRE_TOKEN=ctr_... -- contraire mcp --host http://pi.local:8083   # register
claude mcp list                                        # check that it is connected
```

Then just ask, in your own words:

- *"Which containers on the Pi are unhealthy, and why?"*
- *"Show me the errors in the cart logs from the last restart."*
- *"Restart the shop stack and tell me if everything comes back healthy."*

| Tool | What the agent can do |
|---|---|
| `list_containers`, `list_stacks` | See everything, with plain-language status |
| `inspect_container` | Health streak, exit code explained, ports, mounts |
| `container_logs` | Last lines, optionally filtered by a word |
| `container_stats`, `host_info` | CPU, memory, network, host facts |
| `recent_events`, `activity` | What happened, and who changed what |
| `start_container`, `restart_container`, `stop_container`, `stack_action` | Act (needs operate) |

## Tokens and permissions

| Scope | Allows |
|---|---|
| `read` | Look at everything: containers, logs, stats, images, stacks |
| `operate` | Also start, stop, restart, pause and stack actions |
| `admin` | Also remove, pull, prune, the audit log and managing tokens |

```sh
contraire activity                                     # who changed what (admin token)
contraire activity -n 100                              # the last 100 entries
```

> A token is shown only once, when you create it. Lost it? Revoke it on the **Access** page and create a new one.

## Run the server

```sh
contraire                                              # serve on all interfaces, port 8083
contraire -l 9000                                      # another port
contraire -l 127.0.0.1:8083                            # only this machine
contraire -p /etc/contraire/password                   # require a password in the browser
contraire --audit-file off                             # no audit log (also disables tokens)
contraire --help                                       # every option, with its environment variable
```

```sh
docker run -d -p 8083:8083 -v /var/run/docker.sock:/var/run/docker.sock contraire   # as a container
docker compose up -d                                   # with the compose.yaml from the repository
```

## When something goes wrong

```sh
contraire info                                         # "cannot reach": check the host, port and firewall
ssh -L 8083:localhost:8083 pi                          # reach a Pi outside your network through SSH
```

| Message | What to do |
|---|---|
| `not authorized` | Set `CONTRAIRE_TOKEN`, or create a new token |
| `token scope 'read' does not allow this` | Use a token with a higher scope |
| `cannot reach http://…` | Is the server running? Right port? Same network? |
| `--host must look like http://…` | No HTTPS in the binary: use an SSH tunnel or a VPN |
| exit code `137` | Out of memory or force-stopped: check limits with `inspect` |

## In the dashboard

| Key | Does |
|---|---|
| `/` | Jump to the search box |
| `Esc` | Close the container page, or clear the search |
| `Enter` | Open the focused container |
| `Tab` | Move through every button and link |

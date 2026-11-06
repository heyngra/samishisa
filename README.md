# samishisa (さみしさ)

**samishisa** is a Rust based REST api, powered by extensions.

> [!CAUTION]
> Made for my own needs, it might bug out. Appreciating PRs!

# Installation
```bash
curl -fsSL https://raw.githubusercontent.com/heyngra/samishisa/main/install.sh | sh     # Linux, macOS, WSL
irm https://raw.githubusercontent.com/heyngra/samishisa/main/install.ps1 | iex          # Windows
```

# Usage
```bash
samishisa
# Usage:
#  samishisa run [EXT.wasm ...]         start the server with your config (plus any .wasm files given)
#  samishisa add <SOURCE> [--name N] [--config-json JSON]   add an extension to the config
#  samishisa list                       the extensions in the config
#  samishisa remove <NAME>              take an extension out of the config
#  samishisa config                     the config screen: extensions on/off, their settings, the tunnel
#  samishisa config [path | show | edit]  where the config is / print it / open the file in your editor
#  samishisa config set [EXT] KEY VALUE set a setting: `config set port 8080`, `config set mio anilist.client_id 123`
#  samishisa init [DIR]                 create an api.config.json in DIR (a project folder) to start from
#  samishisa update                     re-resolve remote extensions and move their pins (api.lock.json)
#  samishisa new <NAME> [--sdk PATH]    create an extension crate in ./<NAME>
#  samishisa build [DIR]                compile the extension crate in DIR (default .)
#  samishisa dev [DIR] [--config-json JSON]  build, serve, and rebuild + reload on every change
#  samishisa inspect <EXT.wasm>         show an extension's routes and settings
#  samishisa healthcheck                exit 0 if the local server answers /health (for Docker; uses the config's port)
#  samishisa version | help

#Options:
#  --config FILE    config file (default: ./api.config.json if it exists, else the user config, or $API_CONFIG);
#                   run, add, list, remove, config, update, healthcheck
#  --port N         listen port (default 3000, or $PORT): run, dev, healthcheck
#  --host ADDR      listen address, an IP (default 0.0.0.0 = every interface; 127.0.0.1 = this machine only): run
#  --threads N      serving threads (default: one per CPU core): run
```

To add an extension, it is recommended to use `samishisa conifg`. If you prefer to do it manually, you can edit the config json.

Example JSON:
```json
{
  "port": 3000,
  "extensions": [
    "builtin:hello",
    "./weather.wasm",
    "github:someone/their-extension#v1.2.0",
    "https://example.com/ext.wasm",
    { "source": "github:someone/weather", "name": "wx", "config": { "apiKey": "${WEATHER_KEY}" },
      "network": ["api.weather.example"], "rateLimit": { "limit": 30, "windowSec": 60 } }
  ]
}
```

To learn how to create your own extensions, please study the example `hello` extension.

Example commands to test the hello extension:
```bash
rustup target add wasm32-wasip2
samishisa new hello && cd hello
samishisa dev                                      # build + serve + rebuild/reload on every save
curl 'localhost:3000/hello/Ada?shout=true'        # HELLO, ADA!
curl 'localhost:3000/hello/Ada?shout=maybe'       # 400: query.shout: "maybe" is not of type "boolean"
```


## Development

```bash
rustup target add wasm32-wasip2
cargo build --release --target wasm32-wasip2 -p hello -p probe   # extensions first (hello gets embedded)
cargo build --release -p samishisa-server
cargo test -p samishisa-server && cargo test --doc -p samishisa
python3 tests/e2e.py                                              # REDIS_URL=... adds the Redis checks
```

# rdesk

Acesso remoto pessoal em Rust, para substituir o AnyDesk: **QUIC com hole punching ([iroh](https://www.iroh.computer))** e um **relay privado opcional** no seu servidor, autenticação com **chave SSH do 1Password** (ou senha forte guardada no 1Password) e reconexão automática. Nada a instalar à parte.

- **Host** (máquina controlada): Windows, macOS, Linux (X11).
- **Cliente** (viewer): macOS, Linux, Windows.
- Tela + mouse + teclado, múltiplos monitores, clipboard de texto nos dois sentidos, envio/download de arquivos com retomada.

## Por que não dá timeout como o AnyDesk

- **Conexão direta sempre que possível**: o iroh faz a travessia de NAT (hole punching); o relay só apresenta as pontas e fica de reserva quando o NAT não deixa. Na LAN, nem relay é preciso.
- **QUIC** com keepalive de 1 s e idle timeout de 6 s: um link morto é detectado em segundos, não em minutos.
- **Reconexão automática** (0 → 250 ms → … → 2 s, para sempre) com **token de retomada**: ao voltar a rede você não precisa aprovar o 1Password de novo (vale 10 min).
- **Frames com controle por ACK**: no máximo 2 frames em trânsito; em link ruim cai o fps, não cresce a latência. Tiles perdidos são reenviados sozinhos.
- Tiles 64×64: só o que mudou é enviado; JPEG enquanto se mexe, zstd sem perdas quando para (texto nítido).

## Compilar

```sh
cargo build --release -p rdesk        # host + cliente
cargo build --release -p rdesk --no-default-features --features client   # só cliente
cargo build --release -p rdesk --no-default-features --features host     # só host
```

Linux precisa de: `pkg-config libxcb1-dev libxrandr-dev libxdo-dev libdbus-1-dev libpipewire-0.3-dev libwayland-dev libegl-dev libgbm-dev libxkbcommon-dev libgtk-3-dev clang`.

> **Antivírus (Kaspersky etc.)**: o build executa pequenos programas (build scripts) recém-compilados e sem assinatura, e o próprio `rdesk` captura tela/injeta input/escuta na rede — o mesmo comportamento de um trojan de acesso remoto. Heurísticas podem bloquear. Adicione exclusões para a pasta `target/` do projeto e para o `rdesk.exe` instalado.

## O app (tudo pela interface)

Rode `rdesk` sem argumentos (ou dê duplo clique no executável). Abre uma janela com três abas e um ícone no tray; fechar a janela só a esconde no tray e o app continua rodando, como o AnyDesk. Abrir de novo apenas mostra a janela que já está aberta.

**🖥 Computers** — suas máquinas:
- *My computers*: cada máquina é cadastrada pelo seu **host ID** (mostrado na aba *This computer* dela).
- Status ao vivo: *Ready*, *In use*, inalcançável, com latência e link *direct*/*relay*.
- **Connect** (ou duplo clique) abre a sessão numa janela própria; **Files** abre já no painel de transferência de arquivos (enviar, navegar nas pastas, baixar).
- **＋ Add computer / Edit**: host ID, endereço (opcional com relay; necessário sem ele: IP da LAN ou `host:porta`), chave SSH (*From 1Password…*), e a senha de três jeitos, nesta ordem: referência `op://` do 1Password → senha salva no cofre do sistema (Gerenciador de Credenciais / Keychain) → um diálogo ao conectar, com a opção **Remember**.
- O ID é a própria identidade: a conexão só se completa com a máquina que tem a chave daquele ID, então não há "confiar no fingerprint". Clique direito: copiar endereço/ID, editar, remover.

**🏠 This computer** — o host embutido (deixa este computador ser acessado):
- **Turn on / Turn off** (lembra a escolha), **host ID** com **Copy**, estado do relay e porta UDP para conexões diretas.
- *Not running as administrator* → **Restart as administrator** (Windows), para controlar o Gerenciador de Tarefas e outras janelas de admin.
- Sessão atual (quem, de onde, como) com **Disconnect** — o outro lado não tenta reconectar.
- *Who can log in*: **Set password…** (com *Generate a strong one* e *Read from 1Password*), chaves SSH (**Add SSH key…** do agente do 1Password ou colando a chave pública), remover.
- *Options*: porta UDP, pasta dos arquivos recebidos, fps, qualidade, clipboard. *Activity*: conexões e **logins que falharam**. *Diagnostics*: o mesmo que `rdesk host check`.

**⚙ Settings**: **Network** (o endereço do seu relay; salvar reinicia o host embutido), iniciar com o sistema (*off / ao fazer login / ao fazer login como administrador* — este último cria uma tarefa agendada sem o limite de 3 dias, pedindo permissão uma vez), comportamento do tray, Cmd→Ctrl, **My SSH keys** (copiar a chave pública para autorizar em outra máquina) e a pasta de config/logs.

Tray: *Open rdesk*, estado do host, *Turn host on/off*, *Copy host ID*, *Quit rdesk*.

Logs do app: `<pasta de config>/logs/app.log`; de cada sessão: `logs/session-<nome>.log`.

## Configurar pela linha de comando (opcional)

Tudo o que o app faz também existe como comando; use o que preferir.

### Rede: relay privado (opcional)

Sem relay, o cliente alcança o host só pelo endereço: na LAN, ou pela internet com a porta UDP 7420 redirecionada no roteador. Com um relay (um servidor seu, por exemplo numa VPS), basta o ID, de qualquer lugar:

```sh
rdesk net relay https://relay.seu-dominio.com   # `off` desliga
rdesk net status
```

O mesmo arquivo `network.toml` vale para o host e o cliente da máquina. Nenhum endereço é publicado em DNS ou em servidores de terceiros.

### Host (PC de casa)

1. Inicialize:
   ```sh
   rdesk host init
   ```
   Anote o **host ID** impresso (ou veja depois com `rdesk host id`).
2. Autorize sua chave SSH do 1Password. No **cliente** (Mac/Linux, com o agente SSH do 1Password ligado em *Settings → Developer*):
   ```sh
   rdesk agent-keys
   ```
   Copie a linha `ssh-ed25519 AAAA…` e, no **host**:
   ```sh
   rdesk host add-key "ssh-ed25519 AAAA… comentario"
   ```
   (Se o host também tiver o 1Password: `rdesk host add-key --from-agent`.)
3. (Opcional) Senha de reserva, gerada e guardada no 1Password:
   ```sh
   rdesk host set-password --op "op://Private/rdesk/password"
   ```
   O host guarda só um registro OPAQUE (Argon2id); a senha nunca trafega.
4. Diagnóstico e execução:
   ```sh
   rdesk host check
   rdesk host run
   ```
5. Iniciar com o Windows: pelo app (Settings → *Start when I log in, as administrator*) ou com `rdesk autostart login-admin` num terminal de administrador. Manualmente:
   ```powershell
   schtasks /Create /TN rdesk-host /SC ONLOGON /RL HIGHEST /TR "\"C:\caminho\rdesk.exe\" host run"
   ```
   macOS: crie um LaunchAgent; Linux: um serviço `systemd --user`. Em macOS conceda **Gravação de Tela** e **Acessibilidade** ao `rdesk` (Ajustes → Privacidade e Segurança). Em Linux use sessão **X11**.

Config do host: `%APPDATA%\rdesk\rdesk\config\host.toml` (Windows), `~/Library/Application Support/dev.rdesk.rdesk/` (macOS), `~/.config/rdesk/` (Linux).

```toml
port = 7420              # UDP, para conexões diretas (LAN / porta redirecionada)
max_fps = 30
jpeg_quality = 70
clipboard = true
# inbox_dir = "C:/Users/voce/Downloads/rdesk"
[transport]
keep_alive_ms = 1000
idle_timeout_ms = 6000
congestion = "cubic"     # ou "bbr"
```

### Conectar (Mac/Linux da empresa)

#### Linha de comando

```sh
rdesk hosts add casa <host ID> --password-ref "op://Private/rdesk/password"
rdesk hosts add lab <host ID> --addr 192.168.0.20     # sem relay: endereço direto
rdesk connect casa
rdesk connect <host ID>@192.168.0.20                  # sem cadastrar
```

Ordem de autenticação: token de retomada → chave SSH (agente do 1Password; aprovação/Touch ID) → senha (`op read` ou prompt).

Na janela:
- **▼ rdesk** (topo) abre o menu: monitores, fps/qualidade, tela cheia, Cmd ⇄ Ctrl, Ctrl+Alt+Del, arquivos.
- Arraste arquivos para a janela para enviá-los ao host (vão para `Downloads/rdesk`).
- No Mac, Cmd vira Ctrl no host por padrão (`--no-swap-cmd-ctrl` desliga).

Linha de comando:

```sh
rdesk send casa relatorio.pdf fotos.zip
rdesk ls casa "~/Documents"
rdesk get casa "~/Documents/planilha.xlsx" --out ~/Downloads
rdesk connect casa --headless --frames 300     # teste de link: imprime RTT/fps/kbit/s
```

## Segurança

- Transporte: QUIC/TLS 1.3 do iroh, com a chave Ed25519 do host **como endereço** (host ID): a conexão só se completa com quem tem a chave privada daquele ID. Relay e rede veem apenas tráfego cifrado de ponta a ponta.
- Autenticação **amarrada ao canal TLS** (exporter): assinatura SSHSIG (namespace `rdesk-auth-v1@rdesk`) sobre `{exporter, nonce, host ID, chave, cliente}`; não serve para outra conexão nem para outro protocolo.
- Senha via **OPAQUE** (`opaque-ke`, Ristretto255 + Argon2id 64 MiB).
- Só alcança o host quem sabe o ID (e o endereço, ou usa o seu relay). No máximo 4 handshakes simultâneos, backoff exponencial após falhas (por IP; conexões via relay compartilham um contador), nenhum stream extra antes do login.
- Uma sessão por vez: uma nova sessão autenticada derruba a anterior. Ao desconectar, o host solta todas as teclas/botões.

## Limitações conhecidas

- Com o app rodando como administrador no Windows, as janelas de sessão abertas por ele também são elevadas e o Windows bloqueia arrastar arquivos do Explorer para elas: use *Send files…* no menu da sessão.
- Tela de bloqueio/UAC do Windows não aparece (é o "secure desktop"); exige um serviço SYSTEM (futuro).
- Wayland: captura/input dependem de portais — use X11 no host Linux por enquanto.
- O cursor remoto não é desenhado na imagem; use o cursor local (posição é absoluta e sincronizada).
- Sem áudio. Captura via `xcap` (GDI/CGWindowList/X11): ~20–40 ms por frame em 1080p; DXGI/ScreenCaptureKit/H.264 são o próximo passo.

## Desenvolvimento

```sh
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

Host com tela sintética e input falso (testes, máquinas sem monitor):

```sh
rdesk host run --port 7421 --fake-capture --fake-input
rdesk connect <host ID>@127.0.0.1:7421 --identity ./chave_teste --headless --frames 60 --dump tela.ppm
```

Checklist manual: [docs/TESTING.md](docs/TESTING.md).

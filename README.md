# rdesk

Acesso remoto pessoal em Rust, para substituir o AnyDesk: **QUIC com hole punching ([iroh](https://www.iroh.computer))** e um **servidor privado** seu (contas com Google/GitHub + 2FA, e o relay), autenticação com **chave SSH do 1Password** (ou senha forte guardada no 1Password) e reconexão automática. Nada a instalar à parte.

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

Rode `rdesk` sem argumentos (ou dê duplo clique no executável). Antes de tudo, a janela pede o **login no seu servidor** (endereço → *Sign in with the browser* → Google/GitHub → 2FA); sem login o app não faz nada. Depois abre as abas e um ícone no tray; fechar a janela só a esconde no tray e o app continua rodando, como o AnyDesk. Abrir de novo apenas mostra a janela que já está aberta.

**🖥 Computers** — os computadores que você pode acessar, vindos do servidor (como no TeamViewer):
- *My computers* (os que você cadastrou) e *Shared with me* (os que compartilharam com você ou que estão visíveis para todos).
- Status ao vivo: *Online*, *In use*, *Offline · seen 3 h ago*.
- **Connect** (ou duplo clique) abre a sessão numa janela própria; **Files** abre já no painel de transferência de arquivos (enviar, navegar nas pastas, baixar).
- Nos seus: **Share…** (pessoas e grupos), *Who can see it* (só eu / compartilhado / todos do servidor), *Rename…*, *Unregister…*.
- *Login options…* (clique direito): como responder ao **cadeado extra** de um computador (senha ou chave SSH próprias dele): chave do 1Password, referência `op://`, senha salva no cofre do sistema.

**🏠 This computer** — o host embutido (deixa este computador ser acessado):
- **Turn on / Turn off** (lembra a escolha), **host ID** com **Copy**, estado do relay e porta UDP para conexões diretas.
- *On your rdesk server*: **Register this computer** (nome e quem pode acessar) ou *Unregister…*. Cadastrado, o computador tem credencial própria: continua acessível mesmo sem ninguém logado no app.
- *Not running as administrator* → **Restart as administrator** (Windows), para controlar o Gerenciador de Tarefas e outras janelas de admin.
- Sessão atual (quem, de onde, como) com **Disconnect** — o outro lado não tenta reconectar.
- *This computer's own password and keys* (para a LAN sem servidor, ou como cadeado extra): **Set password…** (com *Generate a strong one* e *Read from 1Password*), chaves SSH (**Add SSH key…** do agente do 1Password ou colando a chave pública), remover.
- *Options*: porta UDP, pasta dos arquivos recebidos, fps, qualidade, clipboard, **cadeado extra** (pedir também a senha ou chave deste computador depois da permissão do servidor). *Activity*: conexões e **logins que falharam**. *Diagnostics*: o mesmo que `rdesk host check`.

**⚙ Settings**: **Account** (com quem você está logado, quantos dias faltam, *Sign out*), iniciar com o sistema (*off / ao fazer login / ao fazer login como administrador* — este último cria uma tarefa agendada sem o limite de 3 dias, pedindo permissão uma vez), comportamento do tray, Cmd→Ctrl, **My SSH keys** (copiar a chave pública para autorizar em outra máquina) e a pasta de config/logs.

**👥 Groups**: grupos de pessoas para compartilhar computadores. Quem cria vira admin do grupo; admins adicionam e removem membros e promovem outros admins.

**🛡 Admin** (só admins do sistema): quais e-mails podem entrar, tornar admin, desativar, remover; e a lista de **todos os computadores** (só visualização: admins acessam apenas o que foi compartilhado com eles).

Tray: *Open rdesk*, estado do host, *Turn host on/off*, *Copy host ID*, *Quit rdesk*.

Logs do app: `<pasta de config>/logs/app.log`; de cada sessão: `logs/session-<nome>.log`.

## Configurar pela linha de comando (opcional)

Tudo o que o app faz também existe como comando; use o que preferir.

### Servidor e login

O servidor privado fica numa VPS sua: veja [docs/SERVER.md](docs/SERVER.md). Em cada computador:

```sh
rdesk login rdesk.seu-dominio.com   # abre o navegador: Google/GitHub + 2FA
rdesk whoami                        # quem está logado, até quando, e se o relay responde
rdesk logout
```

Logado, a lista de computadores e o relay vêm do servidor. A sessão vale 10 dias. Nenhum endereço é publicado em DNS ou em servidores de terceiros.

```sh
rdesk machines register --name "PC da sala" --visibility shared   # neste computador
rdesk machines list
rdesk machines share "PC da sala" --user maria@gmail.com --group Família
rdesk machines visibility "PC da sala" private|shared|everyone
rdesk groups create Família && rdesk groups add Família joao@gmail.com [--admin]
```

### Host (PC de casa)

1. Logado (`rdesk login …`), cadastre o computador: `rdesk machines register` (ou pelo app, aba *This computer*).
2. Execute: `rdesk host run` (ou ligue o host no app). `rdesk host check` mostra o diagnóstico.
3. (Opcional) Cadeado extra, ou acesso pela LAN sem servidor: senha (`rdesk host set-password --op "op://Private/rdesk/password"`, guardada como registro OPAQUE) e/ou chaves SSH (`rdesk host add-key --from-agent` ou `rdesk host add-key "ssh-ed25519 AAAA…"`). Para exigir também esse fator depois da permissão do servidor: `require_local = true` no `host.toml` (ou *Options → Extra lock* no app).
4. Iniciar com o Windows: pelo app (Settings → *Start when I log in, as administrator*) ou com `rdesk autostart login-admin` num terminal de administrador. Manualmente:
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
rdesk connect "PC da sala"               # pelo nome no servidor (ou pelo host ID)
rdesk connect <host ID>@192.168.0.20     # computador não cadastrado, na LAN
```

Ordem de autenticação: token de retomada → **tíquete do servidor** (pedido automaticamente) → se o computador tiver o cadeado extra: chave SSH (agente do 1Password; aprovação/Touch ID) → senha (`op read` ou prompt).

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
- **Tíquetes**: para conectar a um computador cadastrado, o servidor emite um tíquete assinado (Ed25519) dizendo *usuário U pode controlar a máquina M a partir do endpoint E até T* (no máximo 12 h). O host confere sozinho, com a chave do servidor guardada no cadastro: assinatura, máquina, endpoint do outro lado da conexão, validade e revogações. Tirar o acesso de alguém (descompartilhar, mudar a visibilidade, tirar de um grupo, desativar o usuário) revoga na hora: o host recebe pelo sync e derruba a sessão. Com o servidor fora do ar, tíquetes já emitidos valem até expirar.
- **Cadastro com prova de posse**: o pedido de cadastro é assinado com a chave do host, então ninguém cadastra o ID de outra máquina como seu.
- Um computador cadastrado só aceita quem traz tíquete (a senha/chave dele viram cadeado extra opcional). No máximo 4 handshakes simultâneos, backoff exponencial após falhas (por IP; conexões via relay compartilham um contador), nenhum stream extra antes do login.
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

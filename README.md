# rdesk

Acesso remoto pessoal em Rust, para substituir o AnyDesk: **QUIC com hole punching ([iroh](https://www.iroh.computer))** e um **servidor privado** seu (contas com Google/GitHub + 2FA, e o relay), autenticação com **chave SSH do 1Password** (ou senha forte guardada no 1Password) e reconexão automática. Nada a instalar à parte.

- **Host** (máquina controlada): Windows, macOS, Linux (X11).
- **Cliente** (viewer): macOS, Linux, Windows.
- Tela + mouse + teclado, múltiplos monitores, clipboard de texto nos dois sentidos, envio/download de arquivos com retomada, terminal remoto.

**Primeira vez? Leia o [Guia do rdesk](docs/guia/README.md)**: passo a passo para criar o servidor, instalar o app e montar cada caso (usar e ser acessado, só acessado, sem servidor, rede local, Tailscale…), em português simples.

## Por que não dá timeout como o AnyDesk

- **Conexão direta sempre que possível**: o iroh faz a travessia de NAT (hole punching); o relay só apresenta as pontas e fica de reserva quando o NAT não deixa. Na LAN, nem relay é preciso.
- **QUIC** com keepalive de 1 s e idle timeout de 6 s: um link morto é detectado em segundos, não em minutos.
- **Reconexão automática** (0 → 250 ms → … → 2 s, para sempre) com **token de retomada**: ao voltar a rede você não precisa aprovar o 1Password de novo (vale 10 min).
- **Frames com controle por ACK**: no máximo 2 frames em trânsito; em link ruim cai o fps, não cresce a latência. Tiles perdidos são reenviados sozinhos.
- Tiles 64×64: só o que mudou é enviado; JPEG enquanto se mexe, zstd sem perdas quando para (texto nítido).

## Baixar

Do seu servidor: **`https://seu-servidor/download`**. Ele lista a versão liberada para todos:
- **Windows**: `rdesk@seu-servidor.exe`, que já sabe qual servidor usar. Abra, entre e, em *Este computador*, **Instalar** (serviço do Windows) (instala em `Program Files`, o serviço, o Menu Iniciar e *Configurações → Aplicativos*; atualiza sozinho). Pela linha de comando: `rdesk install` (ou `rdesk install --only-share --key rdk_…` para um computador só acessado), `rdesk uninstall`.
- **Linux**: `curl -fsSL https://seu-servidor/install.sh | sudo sh` (instala o `.deb` e liga as atualizações; `… | sudo sh -s -- --host --key rdk_…` para um computador só acessado).
- **Mac**: o `rdesk.app`.

As versões publicadas ficam nos [Releases](../../releases): app para Windows, Linux (x64 e arm64, também em `.deb`) e Mac com Apple Silicon (`rdesk.app`; na primeira vez, botão direito → *Abrir*), e o servidor. Cada release traz um `release.json` assinado; como publicar e conferir: [docs/RELEASE.md](docs/RELEASE.md).

## Compilar

```sh
cargo build --release -p rdesk        # host + cliente
cargo build --release -p rdesk --no-default-features --features client   # só cliente
cargo build --release -p rdesk --no-default-features --features host     # só host
```

Linux precisa de: `pkg-config libxcb1-dev libxrandr-dev libxdo-dev libdbus-1-dev libpipewire-0.3-dev libwayland-dev libegl-dev libgbm-dev libxkbcommon-dev libgtk-3-dev clang`.

> **Antivírus (Kaspersky etc.)**: o build executa pequenos programas (build scripts) recém-compilados e sem assinatura, e o próprio `rdesk` captura tela/injeta input/escuta na rede — o mesmo comportamento de um trojan de acesso remoto. Heurísticas podem bloquear. Adicione exclusões para a pasta `target/` do projeto e para o `rdesk.exe` instalado.

## O app (tudo pela interface)

Rode `rdesk` sem argumentos (ou dê duplo clique no executável). O app fala **português do Brasil ou inglês** (por padrão, o idioma de exibição do sistema; troque em *Configurações → Geral*). Antes de tudo, a janela pede o **login no seu servidor** (endereço → *Entrar pelo navegador* → Google/GitHub → 2FA). Abaixo do login há duas opções sem conta: **Usar nesta rede, sem servidor** (veja [Sem servidor](#sem-servidor-rede-local-e-tailscale)) e **Só compartilhar este computador**, para um computador que só vai **ser acessado**: pede o e-mail de quem vai ser o dono (ou uma chave de cadastro) e espera a aprovação (veja [Computador só acessado](#computador-só-acessado-sem-login-nele)). Depois abre a janela com uma barra lateral e um ícone na bandeja; fechar a janela só a esconde na bandeja e o app continua rodando, como o AnyDesk. Abrir de novo apenas mostra a janela que já está aberta. Avisos e erros aparecem no canto da janela (os erros ficam até você fechar).

**Computadores** — os computadores que você pode acessar, numa lista com um painel de detalhes:
- Um campo **Buscar ou conectar** (Ctrl+K) filtra enquanto você digita (nome, e-mail do dono, ID); Enter conecta ao selecionado, ou a um nome, ID ou `ID@endereço`.
- Filtros: *Todos*, *Meus*, *Compartilhados comigo*, *Nesta rede* (os salvos neste aparelho, para a rede local sem servidor) e *Só online*.
- Status ao vivo em cada linha: *Online*, *Em uso*, *Sessão aberta*, *Offline · visto há 3 h*. Duplo clique ou Enter conecta; as setas mudam a seleção; o botão direito abre as mesmas ações do painel.
- Quando computadores pedem para entrar com o seu e-mail (admins também veem os pedidos sem dono), um aviso no topo leva a **Revisar**: confira o código que cada computador mostra e **Aprove** (nome e quem pode ver) ou **Recuse**.
- O painel do selecionado mostra **Conectar** (abre a sessão numa janela própria; com ela aberta, **Mostrar** a traz para a frente), **Arquivos** e **Terminal** (quando o seu acesso permite), o ID do host, **quem tem acesso**, as **sessões recentes** e o histórico completo, o aviso de acesso direto e, em *Avançado*, as **Opções de login** (como responder ao **cadeado extra** de um computador: chave do 1Password, referência `op://`, senha salva no cofre do sistema) e **Descadastrar** (só o dono). Nos seus (e nos que você co-gerencia), o lápis renomeia (F2). Na janela pequena, os detalhes abrem num diálogo.
- **Gerenciar acesso**: num só lugar, *Só eu*, *Pessoas e grupos* (cada um com *controle total*, *só visualizar*, *só arquivos* ou *só terminal*; o dono também escolhe quem *pode gerenciar*) ou *Todos do servidor*, com o que cada escolha significa.
- **＋ Adicionar**: um assistente com cada forma de incluir um computador (este, outro em que você vai entrar, outro aprovado pelo seu e-mail, vários com **chave de cadastro**, um da rede local sem servidor).

**Este computador** — o host embutido (deixa este computador ser acessado), como um painel:
- O estado com o **interruptor** (lembra a escolha), o **ID do host** com *Copiar*, e por onde chegam até ele (pelo servidor, direto na rede local, porta UDP).
- Quem está conectado agora (e se abriu um terminal, inclusive como administrador), com **Desconectar** — o outro lado não tenta reconectar.
- A lista de configuração: **cadastro no servidor** (cadastrado, o computador tem credencial própria: continua acessível mesmo sem ninguém logado no app), **serviço do Windows** (recomendado: veja [Serviço do Windows](#serviço-do-windows); com ele instalado, mudar as configurações pede *Gerenciar como administrador*), senha própria e chaves SSH.
- Abas: *Visão geral*, *Segurança* (senha própria com *Gerar uma forte* e leitura do 1Password, chaves SSH do agente ou coladas, **cadeado extra**, acesso direto, nova identidade), *Desempenho e arquivos* (porta UDP, pasta dos arquivos recebidos, fps, qualidade, área de transferência, terminal como administrador no Windows), *Atividade* (conexões e **logins que falharam**) e *Diagnóstico* (o mesmo que `rdesk host check`). Mudanças que reiniciam o host esperam você clicar em **Aplicar**.

**A janela de sessão** tem uma barra no topo (em tela cheia ela se esconde e volta ao encostar o mouse em cima): monitor, **qualidade** (*Auto*, *Velocidade*, *Qualidade* ou ajuste fino), **teclas** (Ctrl+Alt+Del e tecla Windows nos computadores Windows, Alt+Tab, teclas de Mac), **Arquivos**, **Terminal**, tela cheia e **Desconectar**. Arquivos e terminal abrem num painel lateral: navegue nas pastas, baixe, envie (ou solte arquivos na janela) e acompanhe as transferências; abra vários terminais em abas (como administrador nos Windows com controle total), selecione e copie o texto (Ctrl+Shift+C). Se a conexão cai, a imagem escurece, o teclado pausa e a janela mostra a reconexão, com *Tentar agora*.

**Configurações**: *Conta* (com quem você está logado, quantos dias faltam, *Sair*), *Geral* (**idioma**, iniciar com o sistema na bandeja — no Windows o host fica a cargo do serviço —, comportamento da bandeja), *Sessões remotas* (teclas de Mac), *Chaves SSH* (copiar a sua chave pública para autorizar em outra máquina), *Chaves de cadastro* e *Avançado* (a pasta de configuração e logs).

**Grupos**: grupos de pessoas para compartilhar computadores. Quem cria vira admin do grupo; admins adicionam e removem membros e promovem outros admins.

**Admin** (só admins do sistema): *Usuários* (quais e-mails podem entrar, tornar admin, desativar, remover), *Computadores* (todos, só para ver — admins acessam apenas o que foi compartilhado com eles — e quais são **de teste**) e *Versões* (liberar uma versão para os de teste ou para todos, atualizar o servidor).

Bandeja: *Abrir o rdesk*, estado do host, *Ligar/Desligar o host*, *Copiar o ID do host*, *Sair do rdesk*.

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
rdesk machines share "PC da sala" --user maria@gmail.com:view --group Família   # :control (padrão), :view, :files
rdesk machines managers "PC da sala" --set joao@gmail.com
rdesk machines history "PC da sala"
rdesk machines visibility "PC da sala" private|shared|everyone
rdesk groups create Família && rdesk groups add Família joao@gmail.com [--admin]
rdesk machines requests                      # computadores esperando a sua aprovação
rdesk machines approve 3F2A-91C0 --name "Pi da sala" --visibility shared
rdesk machines refuse 3F2A-91C0
rdesk machines keys create "Raspberry Pis" --days 30 --group Família:view   # imprime a chave uma vez
rdesk machines keys list
rdesk machines keys revoke "Raspberry Pis"
```

### Computador só acessado (sem login nele)

Um computador que nunca acessa ninguém (o PC de um parente, um Raspberry Pi) não precisa de conta. Ele pede ao servidor para entrar, provando que tem a própria chave, e só vira computador do servidor quando:

- **o dono aprova** (pelo e-mail): no app, *Só compartilhar este computador* na tela de login; ou

  ```sh
  rdesk host enroll --server rdesk.seu-dominio.com --owner voce@gmail.com [--name "PC da vó"]
  rdesk host run
  ```

  O host mostra um código (ex.: `3F2A-91C0`) e fica esperando. O dono vê o pedido em *Computadores* (o aviso *Revisar* no topo), confere o código e aprova; o host entra na hora. Sem `--owner`, quem aprova são os admins do servidor. O servidor responde igual para e-mails que não existem e limita os pedidos pendentes (10 por pessoa, 100 no total); um pedido recusado fica recusado por 30 dias.
- **ou com uma chave de cadastro**, sem aprovação: crie em *Computadores → Adicionar → Vários computadores* (ou *Configurações → Chaves de cadastro*) (ou `rdesk machines keys create`) e use `rdesk host enroll --server … --key rdk_…`. A chave tem validade (até 365 dias), pode ser de uso único ou reutilizável, e define quem vê os computadores que ela cadastra (inclusive grupos). Revogar a chave não tira os computadores já cadastrados. Um computador descadastrado não volta sozinho com a chave: ele vira um pedido de aprovação para o dono da chave.

As variáveis `RDESK_SERVER`, `RDESK_OWNER`, `RDESK_ENROLL_KEY` e `RDESK_NAME` fazem o mesmo que `host enroll` e têm prioridade sobre o `host.toml`. A chave é apagada do `host.toml` depois de usada.

**Imagem pronta (Raspberry Pi, Orange Pi…)**:

1. Instale o `rdesk` na imagem (compilado no próprio Pi ou para `aarch64-unknown-linux-gnu`), com sessão gráfica X11 e login automático.
2. **Não** inclua a pasta de config do rdesk (`~/.config/rdesk/`) na imagem: cada placa cria o próprio host ID no primeiro boot. Placas com a mesma pasta teriam o mesmo ID.
3. Crie uma chave reutilizável (ex.: 30 dias, compartilhada com o grupo Família) e coloque em `~/.config/rdesk-enroll.env` (permissão 600):
   ```sh
   RDESK_SERVER=rdesk.seu-dominio.com
   RDESK_ENROLL_KEY=rdk_…
   ```
4. Instale o serviço de usuário [`deploy/rdesk-host.service`](deploy/rdesk-host.service) (`systemctl --user enable rdesk-host`).
5. No primeiro boot cada placa aparece em *My computers*. Terminada a leva, revogue a chave.

### Host (PC de casa)

1. Logado (`rdesk login …`), cadastre o computador: `rdesk machines register` (ou pelo app, em *Este computador*). Sem login nele: `rdesk host enroll` (veja [Computador só acessado](#computador-só-acessado-sem-login-nele)).
2. Execute: `rdesk host run` (ou ligue o host no app). `rdesk host check` mostra o diagnóstico.
3. (Opcional) Cadeado extra, ou acesso pela LAN sem servidor: senha (`rdesk host set-password --op "op://Private/rdesk/password"`, guardada como registro OPAQUE) e/ou chaves SSH (`rdesk host add-key --from-agent` ou `rdesk host add-key "ssh-ed25519 AAAA…"`). Para exigir também esse fator depois da permissão do servidor: `require_local = true` no `host.toml` (ou *Options → Extra lock* no app).
4. Iniciar com o sistema: no Windows, instale o serviço (abaixo). macOS: crie um LaunchAgent; Linux: um serviço `systemd --user`. Em macOS conceda **Gravação de Tela** e **Acessibilidade** ao `rdesk` (Ajustes → Privacidade e Segurança). Em Linux use sessão **X11**.

### Serviço do Windows

O serviço `rdesk` roda como SYSTEM e mantém o host rodando na sessão de quem está na frente da tela:

- o computador fica acessível **antes de alguém entrar no Windows**, na **tela de bloqueio** e nos prompts do **UAC** (o host acompanha a área de trabalho ativa);
- enquanto alguém está conectado, uma barra no topo da tela mostra quem é, com o botão **Encerrar**;
- arquivos enviados e baixados passam pelos direitos de quem está logado (vão para a pasta Downloads dessa pessoa); sem ninguém logado, arquivos são recusados;
- segue só a sessão do console (a tela física), não sessões de Área de Trabalho Remota.

```powershell
rdesk service install     # num terminal de administrador (ou o botão em Este computador)
rdesk service status
rdesk service uninstall   # os arquivos do host voltam para a sua pasta
```

A instalação copia o programa para `C:\Program Files\rdesk` e move a identidade, o cadastro e as configurações do host para `C:\ProgramData\rdesk` (só SYSTEM e Administradores leem); o host ID não muda. Logs: `C:\ProgramData\rdesk\logs\service.log` e `host.log`. Reinstalar (para atualizar o programa) é rodar `rdesk service install` de novo.

Config do host: `%APPDATA%\rdesk\rdesk\config\host.toml` (Windows, ou `C:\ProgramData\rdesk` com o serviço), `~/Library/Application Support/dev.rdesk.rdesk/` (macOS), `~/.config/rdesk/` (Linux).

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

### Sem servidor (rede local e Tailscale)

Para usar antes de ter a VPS, ou entre computadores da mesma casa: cada computador entra com a **própria senha ou chave SSH** (*Este computador → Segurança*), e só a partir da **rede local** ou do **Tailscale**.

- No app, na tela de login: **Usar nesta rede, sem servidor**. Em *Computadores*, o filtro *Nesta rede* mostra os salvos neste aparelho; **＋ Adicionar → Um computador nesta rede** pede nome, host ID e endereço (IP, nome ou nome do Tailscale; vazio = achar pelo mDNS). Conectado ao servidor, eles aparecem também, na mesma lista.
- Linha de comando, sem login: `rdesk connect <host ID>@192.168.0.20` (ou `@pc.tailnet.ts.net`), ou `rdesk connect <host ID>` com o mDNS ligado no host.

No host (*Este computador → Segurança → Acesso direto*, ou `[direct]` no `host.toml`):

```toml
[direct]
enabled = true                 # padrão: ligado sem servidor, desligado depois de cadastrado
from = ["lan", "tailscale"]    # de onde aceita
mdns = false                   # anunciar na rede local (conectar só pelo host ID)
```

- **lan**: endereços privados, link-local, loopback e qualquer endereço numa rede das interfaces do computador (inclui o IPv6 global da mesma /64, comum no Brasil). **tailscale**: 100.64.0.0/10 e fd7a:115c:a1e0::/48.
- Qualquer outra origem é recusada **antes** do login, e o relay nunca entra por aqui. Com o serviço do Windows, uma regra do firewall também só deixa chegar conexões da rede local e do Tailscale (as abertas pelo próprio host, como o hole punching, continuam funcionando).
- Num computador **cadastrado**, ligar o acesso direto abre uma segunda porta além do compartilhamento do servidor: o app de quem vê o computador mostra *⚠ direct access on*, e essas sessões entram no histórico.
- O mDNS anuncia só os IPs (nunca o relay); quem está na rede vê que há um host rdesk ali.

### Conectar (Mac/Linux da empresa)

#### Linha de comando

```sh
rdesk connect "PC da sala"               # pelo nome no servidor (ou pelo host ID)
rdesk connect <host ID>@192.168.0.20     # computador não cadastrado, na LAN
```

Ordem de autenticação: token de retomada → **tíquete do servidor** (pedido automaticamente) → se o computador tiver o cadeado extra: chave SSH (agente do 1Password; aprovação/Touch ID) → senha (`op read` ou prompt).

Na janela (detalhes em [Usar no dia a dia](docs/guia/05-usar.md)):
- A **barra do topo** tem monitor, qualidade, teclas (Ctrl+Alt+Del, tecla Windows, Alt+Tab, teclas de Mac), **Arquivos**, **Terminal** (e terminal como administrador, no Windows), tela cheia e **Desconectar**.
- Arraste arquivos para a janela para enviá-los ao host (vão para `Downloads/rdesk`).
- No Mac, por padrão, Cmd vira Ctrl, o Option esquerdo vira a tecla Windows e o Control vira Alt; o Option direito continua AltGr (`--no-swap-cmd-ctrl` desliga).
- Minimizada, a janela pausa o vídeo; ao voltar, recebe a tela inteira de novo.

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
- Um computador cadastrado só aceita quem traz tíquete (a senha/chave dele viram cadeado extra opcional), a não ser que o acesso direto esteja ligado nele: aí a senha/chave dele também abrem, só da rede local ou do Tailscale. Sem servidor, conexões de fora da rede local e do Tailscale são recusadas antes do login. No máximo 4 handshakes simultâneos, backoff exponencial após falhas (por IP; conexões via relay compartilham um contador), nenhum stream extra antes do login.
- Uma sessão por vez: uma nova sessão autenticada derruba a anterior. Ao desconectar, o host solta todas as teclas/botões.

## Limitações conhecidas

- Com o app rodando como administrador no Windows (*Gerenciar como administrador*), as janelas de sessão abertas por ele também são elevadas e o Windows bloqueia arrastar arquivos do Explorer para elas: use *Enviar arquivos…* no painel de arquivos da sessão.
- Tela de bloqueio/UAC do Windows e Ctrl+Alt+Del: só com o serviço instalado.
- Wayland: imagem e input vão pelo portal do desktop; na primeira conexão alguém no computador precisa permitir (depois a permissão fica guardada, no GNOME 46+ e no KDE Plasma 6.1+). Ainda não testado num desktop de verdade: se der problema, `RDESK_WAYLAND=0` volta ao X11.
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

## Licença

Projeto pessoal, sem licença por enquanto: o código está visível, mas todos os direitos são reservados.

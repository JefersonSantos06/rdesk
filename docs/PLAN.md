# Plano: rdesk com servidor privado e contas

Uso interno (família): ninguém de fora entra, nada é público. Funciona como o TeamViewer com conta: cada usuário cadastra as próprias máquinas e decide quem pode acessá-las. A diferença é que o servidor é seu.

## Decisões tomadas

| Tema | Decisão |
|---|---|
| Quem pode ter conta | Só e-mails pré-cadastrados por um admin do servidor |
| Login | OAuth com Google ou GitHub (e-mail verificado, igual ao cadastrado) |
| 2FA | Obrigatório em **todo** login: TOTP e/ou passkey |
| Sessão no app | Expira 10 dias após o login, use ou não: depois disso, login de novo (com 2FA) |
| Admins do sistema | Veem todas as máquinas, mas só acessam o que foi compartilhado com eles |
| Usar o app sem login | Não é possível |
| Máquinas | Cada máquina é cadastrada por um usuário; três níveis de acesso: privada, compartilhada com usuários e grupos específicos, ou visível a todos do servidor |
| Grupos | Qualquer usuário cria grupos; membros têm cargo *admin* ou *membro*; admins do grupo gerenciam membros e cargos (sem "dono") |
| Servidor fora do ar | Tíquetes já emitidos valem por até 12 h |
| Senha / chave SSH no host | Opcionais: cadeado extra por máquina |
| Uma máquina na rede | Só é acessada. Para acessar outras, é preciso uma conta de usuário liberada pelo admin |
| Código e versões | Repositório público no GitHub, **sem licença** por enquanto (código visível, todos os direitos reservados); versões nos GitHub Releases |
| Atualizações | Automáticas, mas só depois que um admin libera no app: primeiro para máquinas de teste, depois para todas. O servidor atualiza por um clique próprio |
| Docker | Continua existindo, **sem** atualização automática |
| Host no Windows | Vira um serviço (SYSTEM): acessível na tela de login, no bloqueio e no UAC; segue só a sessão do console (não RDP) |
| Quem está na frente do PC | Vê um aviso durante a sessão e pode encerrá-la; não precisa aceitar a conexão; sem ser admin, não desliga o acesso |
| Placas (Pi) | Raspberry Pi OS ou Armbian normais + script de instalação + arquivo de configuração na partição de boot; sem imagens próprias |
| Terminal | Acesso ao terminal da máquina pelo app e pela CLI |

## Como funciona

```
          VPS: rdesk-server
  ┌───────────────────────────────────────────┐
  │ Login web: Google/GitHub + 2FA            │
  │ API: usuários, grupos, máquinas, acessos  │
  │ Emissor de tíquetes (chave Ed25519)       │
  │ Relay iroh: só máquinas e sessões válidas │
  │ SQLite                                    │
  └───────────────────────────────────────────┘
        ▲ app logado                ▲ credencial da máquina
   cliente rdesk ◄═══ direto (hole punching) ou via relay ═══► host rdesk
```

1. **Entrar no app**: o app abre o navegador na página do servidor (OAuth com PKCE e retorno para `127.0.0.1`). O servidor confere se o e-mail está na lista e pede o 2FA em todo login (no primeiro, cadastra o TOTP ou a passkey). O app recebe uma sessão de dispositivo revogável, que expira 10 dias após o login.
2. **Cadastrar uma máquina** (aba *This computer*): um usuário logado registra o ID iroh do host, um nome e o nível de acesso. A máquina recebe uma **credencial própria**: continua acessível depois de reiniciar, sem ninguém abrir o app, e guarda a chave pública do servidor.
3. **Lista de computadores**: vem do servidor, filtrada pelo que cada usuário pode acessar. O status *online* vem do relay, que sabe quais máquinas estão conectadas. A lista inclui os endereços de LAN que a máquina informa, para conexão direta.
4. **Conectar**: o cliente pede um **tíquete** ao servidor. O tíquete diz que o usuário U pode controlar a máquina M a partir do ID iroh desta janela até a hora T (no máximo 12 h). O servidor confere as permissões e assina.
5. **O host confere o tíquete sozinho**: a assinatura do servidor, se o tíquete é para ele, se o ID do outro lado da conexão é o do tíquete e se não expirou nem foi revogado. Se a máquina tiver o cadeado extra, pede também a senha ou a chave SSH local.
6. **Revogação**: o servidor avisa o host pela conexão que o host mantém com ele. O host derruba a sessão e passa a recusar aquele tíquete. Com o servidor fora do ar, tíquetes já emitidos valem até expirar.
7. **Relay**: só aceita máquinas cadastradas (credencial) e apps com sessão válida (token).

### Quem acessa o quê

Um usuário acessa uma máquina se for **gestor** dela (quem cadastrou e os co-gestores) ou se:
- a máquina for **"todos"**; ou
- for **compartilhada** com ele diretamente ou com um grupo do qual ele é membro.

Admins do sistema gerenciam a lista de e-mails e os usuários e **veem todas as máquinas** (nome, quem cadastrou, nível de acesso, status). Mas só **acessam** as que foram compartilhadas com eles, pela mesma regra de qualquer usuário. Cada compartilhamento guarda um *papel*: *full control*, *view only* ou *files only* (e, na fase 10, *terminal only*).

## Fases

### Fase 1: transporte iroh (feita, branch `iroh-transport`)
- iroh com hole punching; a máquina é identificada pelo host ID; relay configurável; teste ponta a ponta pelo relay e direto.
- Continua sendo a base. O que muda depois: o relay e a lista de computadores passam a vir do servidor, em vez de configuração manual.

### Fase 2: servidor e login (feita, branch `iroh-transport`)
Guia de instalação: [SERVER.md](SERVER.md). Diferenças em relação ao previsto:
- OAuth implementado direto (PKCE), sem o crate `oauth2`, para não duplicar o `reqwest`.
- Passkeys exigem que o servidor seja acessado por um domínio (com IP, só TOTP).
- Relay e API dividem a porta 443: o relay roda em loopback e o servidor repassa `/relay`.
- Até a fase 3, o host embutido entra no relay com a sessão do usuário (sem login, só LAN).

Planejado:
- Novo crate `rdesk-server`:
  - banco SQLite (usuários, e-mails permitidos, identidades OAuth, TOTP, passkeys, dispositivos, sessões, grupos, máquinas, compartilhamentos, revogações, auditoria);
  - CLI de admin: `rdesk-server init --domain … --admin <e-mail>`, `rdesk-server users add|remove|list`;
  - HTTPS com Let's Encrypt;
  - relay iroh embutido, com controle de acesso.
- Login web: Google e GitHub, TOTP e passkey (WebAuthn), cadastro do 2FA no primeiro acesso, sessões de dispositivo.
- App: tela de login que bloqueia o resto; `rdesk login <servidor>` e `rdesk logout`. O endereço do relay vem do servidor, e `rdesk net relay` sai.
- Gestão dos e-mails pelo app (página *Admin*, só para admins do sistema).

### Fase 3: máquinas, grupos e acesso (feita, branch `iroh-transport`)
Como ficou:
- Tíquetes assinados com a chave do servidor, presos à máquina e ao endpoint do cliente (até 12 h); protocolo v2 (`ClientMsg::Ticket`, `HostMsg::TicketOkNeedLocal`).
- Revogação: o servidor registra quem perdeu acesso e acorda o long-poll do host (`/api/agent/sync`), que derruba a sessão na hora.
- Cadastro com prova de posse (assinatura com a chave do host).
- O status *em uso* e os endereços de LAN vêm do sync do host; *online* vem do relay.
- A lista local de computadores saiu; `client.toml` guarda só as *Login options* (cadeado extra) por host ID.

Planejado:
- API e app:
  - cadastrar e descadastrar a máquina;
  - nível de acesso (privada / específicos / todos);
  - compartilhar com usuários e grupos;
  - páginas *Grupos* (criar, membros, cargos) e *Computers* vinda do servidor, com status online;
  - na página *Admin*, a lista de todas as máquinas (só visualização).
- Tíquetes: emissão no servidor e verificação no host. Novo passo no protocolo (`TicketAuth`); senha e chave SSH viram o cadeado extra opcional.
- Credencial da máquina, chave do servidor guardada no host, revogação empurrada para o host.
- CLI: `rdesk machines`, `rdesk machine register|share|visibility`, `rdesk groups …`.
- Remoções:
  - lista local de computadores (`client.toml` `[hosts]`) e conexão manual por ID;
  - restos da era Tailscale (`host_key.der`, `host_cert.der`, `known_hosts`).

### Fase 4: acabamento (feita, branch `iroh-transport`)
- Papéis por compartilhamento (*full control*, *view only*, *files only*): viram permissões no tíquete (tela, input, arquivos), aplicadas pelo host; reduzir o acesso também revoga.
- Co-gestores de máquina (escolhidos pelo dono) e histórico de acessos (reportado pelo host no sync).
- `rdesk-server backup` e `rotate-key` (hosts recebem a chave nova no sync e aceitam a antiga por 12 h); `Dockerfile`, `deploy/docker-compose.yml` e `deploy/rdesk-server.service`.
- O host não responde mais ao `Probe` (nada antes do login).
- Primeira conexão: medida em ~25 ms do bind ao login pelo relay local; o app agora cria o endpoint e pede o tíquete em paralelo.

### Fase 5: computadores sem login (feita, branch `iroh-transport`)
- O host pede para entrar (`POST /api/enroll`, assinado com a chave do host), sem conta:
  - com o **e-mail do dono**: vira um pedido que o dono aprova no app, conferindo um código (os 8 primeiros dígitos do host ID). Sem e-mail, os admins aprovam. O servidor responde igual para e-mails inexistentes e limita os pedidos (10 por pessoa, 100 no total);
  - com uma **chave de cadastro** (`rdk_…`, criada no app, com validade, uso único ou reutilizável, visibilidade e grupos): entra na hora.
- O host espera a aprovação num long poll e entra sem reiniciar; recebe o token de máquina só depois de aprovado.
- Um computador descadastrado não volta sozinho com uma chave: vira pedido de aprovação.
- Configuração por `rdesk host enroll`, `[enroll]` no `host.toml` ou variáveis de ambiente (`RDESK_SERVER`, `RDESK_OWNER`, `RDESK_ENROLL_KEY`, `RDESK_NAME`); o host cria o próprio ID no primeiro boot.
- App: *Only share this computer* na tela de login; *Waiting for your approval* e *Add a computer…* (chaves) na aba Computers. CLI: `rdesk machines requests|approve|refuse|keys`.

### Ordem das próximas fases
6 → 7 → 8 → 9, porque tudo usa os arquivos que a fase 6 publica e o atualizador do Windows mora no serviço (fase 7). A fase 10 (terminal) depende só da fase 7 e pode vir logo depois dela.

### Fase 6: repositório aberto e versões publicadas (preparada, branch `iroh-transport`)
Como ficou: [RELEASE.md](RELEASE.md). Histórico varrido sem segredos; `rdesk-release` (chaves, manifesto, assinatura, verificação); `release.yml`; verificação no app em `rdesk_common::release`. Falta você: guardar as chaves, criar o secret, tornar o repositório público e publicar a primeira tag. O instalador do Windows ficou para a fase 9.

Planejado:
- Antes de abrir o repositório:
  - varrer todo o histórico em busca de segredos (ex.: `gitleaks`);
  - usar o e-mail `noreply` do GitHub nos próximos commits (os antigos continuam com o e-mail atual);
  - revisar o README para quem chega de fora;
  - sem licença por enquanto.
- Chave de assinatura das versões (Ed25519), gerada fora da VPS:
  - a privada fica nos *Secrets* do GitHub, com cópia no 1Password;
  - os binários levam a pública e uma reserva, para dar para trocar a principal.
  - Assim, quem tomar a VPS, o GitHub ou o caminho do download não consegue entregar um binário alterado.
- `release.yml`, disparado por tag `v*`:
  - compila Windows x64, Linux x64 e Linux arm64 (runner ARM nativo, gratuito em repositório público) e o servidor x64 e arm64;
  - gera o instalador do Windows, o pacote de atualização, os `.deb` (amd64 e arm64), o binário do servidor e o `release.json` (versão, versão de protocolo e SHA-256 de cada arquivo) com a assinatura dele;
  - publica tudo num GitHub Release. A versão vem do `Cargo.toml` do workspace e precisa bater com a tag.
- O macOS continua sendo testado no CI, mas fica fora dos releases.
- Windows sem certificado de assinatura de código: o SmartScreen avisa só na primeira instalação (o que o app baixa sozinho não passa por ele).

### Fase 7: serviço do Windows
- Serviço `rdesk` (SYSTEM), instalado em `C:\Program Files\rdesk`:
  - guarda a identidade da máquina, o cadastro, o endpoint iroh, o sync e as atualizações;
  - dados em `C:\ProgramData\rdesk`, legíveis só por SYSTEM e Administradores;
  - a máquina fica acessível logo depois de reiniciar, antes de alguém entrar no Windows.
- Agente de tela na sessão do **console** (não segue sessões RDP):
  - lançado pelo serviço com token de SYSTEM, para acompanhar a troca de área de trabalho: tela de login, bloqueio, UAC, Gerenciador de Tarefas;
  - relançado quando o usuário sai, troca de conta ou bloqueia a tela.
- Arquivos recebidos são gravados como o usuário logado, na pasta Downloads dele.
- Aviso na tela durante a sessão ("Fulano está conectado", com botão *Encerrar*). Não há pedido para aceitar a conexão. Sem ser admin, o usuário local não desliga o acesso.
- App da bandeja: cliente e painel do host. Conversa com o serviço por um named pipe com controle de acesso; ligar e desligar o host exige admin.
- Saem: *Restart as administrator*, a tarefa agendada *login-admin* e o aviso de bloqueio por UAC.
- Migração automática no instalador: `host_id.key`, `machine.json` e `host.toml` passam do perfil do usuário para `ProgramData`, com o mesmo host ID (ninguém recadastra); o início automático antigo é removido.
- Linux: o host continua como serviço do usuário na sessão X11 (`deploy/rdesk-host.service`), sem tela de login; ganha o mesmo aviso de sessão.

### Fase 8: atualização automática
- Servidor como política, GitHub como armazenamento:
  1. O rdesk-server consulta os Releases de hora em hora (API pública, sem token), confere a assinatura e mostra na aba *Admin* a versão nova, com o changelog.
  2. Dois cliques: **Liberar para máquinas de teste** (marcadas pelo admin) e depois **Liberar para todos**.
  3. O servidor passa a informar a versão liberada no sync dos hosts e para os apps logados. Um endpoint público informa só o número da versão liberada (para o atualizador do Linux).
  4. Cada máquina baixa do GitHub, confere a assinatura e o hash, **espera não haver sessão aberta**, troca e reinicia.
  5. Os hosts informam a versão no sync; a aba *Admin* mostra quem atualizou e quem ficou para trás.
- Windows: o serviço lança um aplicador separado que para o serviço, troca os arquivos e o inicia de novo. Se a versão nova não falar com o servidor em 2 minutos, ele volta para a anterior. A bandeja reabre na versão nova.
- Linux: `rdesk-update` (serviço e timer como root) instala o `.deb`; o host percebe o binário novo e reinicia quando estiver ocioso.
- Servidor: clique próprio no *Admin* (**Atualizar servidor**), feito antes de liberar as máquinas. Faz backup do banco antes, guarda o binário anterior e volta para ele (`OnFailure=` do systemd) se o novo não subir.
- Docker: sem atualização automática. O *Admin* só avisa que existe versão nova; quem usa Docker atualiza a imagem por conta própria.
- Compatibilidade:
  - o servidor atualiza primeiro e continua aceitando apps antigos;
  - cliente e host aceitam o protocolo atual e o anterior;
  - o servidor informa uma versão mínima, abaixo da qual o app pede para atualizar;
  - voltar de versão só por ordem explícita (impede que sirvam uma versão antiga assinada com falha conhecida).

### Fase 9: instaladores
- Windows (Inno Setup):
  - modos **App completo** (cliente, host e bandeja) e **Só ser acessado** (só o serviço, com o e-mail do dono ou uma chave de cadastro da fase 5);
  - o endereço do servidor vem no nome do arquivo (`rdesk-setup@rdesk.seu-dominio.com.exe`);
  - modo silencioso para automação: `/VERYSILENT /SERVER=… /KEY=…`.
- Linux: `install.sh` servido pelo próprio servidor, já com o endereço preenchido (`curl -fsSL https://rdesk.seu-dominio.com/install.sh | sh -s -- --host --key …`). Detecta a arquitetura e instala o `.deb` e os serviços.
- Placas: Raspberry Pi OS ou Armbian normais, com o script e um `rdesk.toml` na partição de boot, lido no primeiro boot.
- Servidor: `install-server.sh` nos Releases. Pergunta só o domínio e o e-mail do admin; cria o usuário, a unit e as regras de firewall, roda o `init` e mostra as URLs para o Google e o GitHub.
- Página `/download` no servidor, com os instaladores da versão liberada já apontando para ele.

### Fase 10: terminal remoto
- App: botão **Terminal** na aba *Computers* e no menu da sessão. Abre uma janela de terminal dentro do app (emulador embutido; a avaliar: `egui_term`, baseado no `alacritty_terminal`). Várias abas por conexão.
- CLI: `rdesk shell "PC"` usa o próprio terminal local.
- Host: um pseudo-terminal (ConPTY no Windows, pty no Linux; a avaliar: `portable-pty`) num stream próprio da conexão iroh, com dados e redimensionamento.
- Como quem roda:
  - Windows: como o usuário logado no console (o serviço obtém o token dele); sem ninguém logado, o terminal é recusado. Shell padrão: PowerShell.
  - Linux: como o usuário do host, com o shell dele.
- Permissão:
  - nova permissão `terminal` no tíquete (protocolo v3);
  - *full control* inclui o terminal (o teclado já dá acesso equivalente);
  - novo papel **terminal only**, para servidores e placas sem tela;
  - *view only* e *files only* não incluem.
- Host sem tela (placa sem X11): aceita sessões só de terminal e arquivos.
- O histórico registra "terminal", e o aviso da fase 7 mostra "Fulano abriu um terminal".

## Ainda não verificado
- Imagem Docker (o `Dockerfile` não foi construído aqui) e a unit do systemd numa VPS de verdade.
- Login real com Google e GitHub (precisa das credenciais OAuth no seu domínio).
- Passkey num navegador de verdade (precisa do servidor num domínio com HTTPS).
- Certificado Let's Encrypt na VPS.
- Imagem de Raspberry/Orange Pi com `deploy/rdesk-host.service` (compilação para ARM, sessão X11 com login automático).

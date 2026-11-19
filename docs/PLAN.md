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

Um usuário acessa uma máquina se for **gestor** dela (quem cadastrou, e futuramente co-gestores) ou se:
- a máquina for **"todos"**; ou
- for **compartilhada** com ele diretamente ou com um grupo do qual ele é membro.

Admins do sistema gerenciam a lista de e-mails e os usuários e **veem todas as máquinas** (nome, quem cadastrou, nível de acesso, status). Mas só **acessam** as que foram compartilhadas com eles, pela mesma regra de qualquer usuário. Cada compartilhamento guarda um *papel*: por enquanto só "controle total"; "só ver" e "só arquivos" ficam para depois.

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

## Ainda não verificado
- Imagem Docker (o `Dockerfile` não foi construído aqui) e a unit do systemd numa VPS de verdade.
- Login real com Google e GitHub (precisa das credenciais OAuth no seu domínio).
- Passkey num navegador de verdade (precisa do servidor num domínio com HTTPS).
- Certificado Let's Encrypt na VPS.
- Imagem de Raspberry/Orange Pi com `deploy/rdesk-host.service` (compilação para ARM, sessão X11 com login automático).

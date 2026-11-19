# Servidor privado (`rdesk-server`)

Um binário só, na sua VPS:

- **Contas**: só e-mails que um admin cadastrou; login com Google ou GitHub e **2FA obrigatório em todo login** (TOTP ou passkey; o cadastro do 2FA acontece no primeiro login). A sessão do app vale **10 dias**.
- **Relay iroh**: só aceita quem está logado. Fica na mesma porta 443, em `/relay`.
- **HTTPS automático** com Let's Encrypt.
- Banco **SQLite** na pasta de dados.

## Requisitos

- Uma VPS Linux com IP público e um **domínio** apontando para ela (ex.: `rdesk.seu-dominio.com`). As passkeys exigem domínio, não funcionam com IP.
- Portas abertas: **443/tcp** (site, API e relay) e **7842/udp** (descoberta de endereço do relay, ajuda o hole punching).

## Instalação

```sh
cargo build --release -p rdesk-server
sudo install -m 755 target/release/rdesk-server /usr/local/bin/
sudo rdesk-server init --public-url https://rdesk.seu-dominio.com --admin voce@gmail.com
sudo rdesk-server info        # mostra as URLs de retorno para cadastrar no Google/GitHub
```

Os dados ficam em `/var/lib/rdesk-server` (`--data` ou `RDESK_SERVER_DATA` mudam isso): `server.toml`, `rdesk.db`, `server.key` e `acme/` (certificados).

### Login com Google

1. Em [console.cloud.google.com](https://console.cloud.google.com/) → *APIs e serviços* → *Credenciais* → *Criar credenciais* → *ID do cliente OAuth* → tipo **Aplicativo da Web**.
2. *URIs de redirecionamento autorizados*: `https://rdesk.seu-dominio.com/login/oauth/google/callback`.
3. `sudo rdesk-server oauth google --client-id … --client-secret …`

### Login com GitHub

1. GitHub → *Settings* → *Developer settings* → *OAuth Apps* → *New OAuth App*.
2. *Authorization callback URL*: `https://rdesk.seu-dominio.com/login/oauth/github/callback`.
3. `sudo rdesk-server oauth github --client-id … --client-secret …`

O GitHub entra com qualquer e-mail **verificado** da conta que esteja na lista.

### Rodar como serviço (systemd)

```ini
# /etc/systemd/system/rdesk-server.service
[Unit]
Description=rdesk server
After=network-online.target
Wants=network-online.target

[Service]
ExecStart=/usr/local/bin/rdesk-server run
Restart=always
RestartSec=3
AmbientCapabilities=CAP_NET_BIND_SERVICE
StateDirectory=rdesk-server

[Install]
WantedBy=multi-user.target
```

```sh
sudo systemctl enable --now rdesk-server
journalctl -u rdesk-server -f
```

## Usuários

Pela VPS:

```sh
rdesk-server users list
rdesk-server users add maria@gmail.com [--admin]
rdesk-server users admin maria@gmail.com on|off
rdesk-server users disable maria@gmail.com on|off   # bloqueia e encerra as sessões
rdesk-server users remove maria@gmail.com
```

Ou pelo app, na aba **Admin** (só para admins do sistema), ou pela CLI do app: `rdesk users list|add|remove|admin|disable`. Sempre fica pelo menos um admin.

## No app

- O app só abre a tela de login até a pessoa entrar: endereço do servidor → **Sign in with the browser** → Google/GitHub → 2FA → volta para o app.
- Linha de comando: `rdesk login rdesk.seu-dominio.com`, `rdesk whoami`, `rdesk logout`.
- Logado, o relay do servidor é usado automaticamente: basta o host ID para conectar.

## Teste local (sem domínio)

```sh
rdesk-server --data ./srv init --public-url http://127.0.0.1:8080 --admin voce@exemplo.com --no-tls
# em ./srv/server.toml: dev_login = true   (login de teste digitando o e-mail; o 2FA continua obrigatório)
rdesk-server --data ./srv run
rdesk login http://127.0.0.1:8080
```

O `dev_login` só vale com `tls = "none"` e endereço local; em produção ele é ignorado.

## Computadores e acesso

- Cada computador é cadastrado por um usuário (no app, aba *This computer*, ou `rdesk machines register`), com prova de posse da chave do host. Ele ganha uma credencial própria: usa o relay e sincroniza com o servidor mesmo sem ninguém logado.
- Níveis: **só eu**, **compartilhado** (com pessoas e/ou grupos) ou **todos do servidor**.
- Para conectar, o app pede um **tíquete** (válido por até 12 h); o host confere sozinho. Tirar o acesso revoga na hora e derruba a sessão em andamento.
- Admins do sistema veem todos os computadores (aba *Admin*), mas só acessam os compartilhados com eles.

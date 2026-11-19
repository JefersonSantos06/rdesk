# Servidor privado (`rdesk-server`)

Um binário só, na sua VPS:

- **Contas**: só e-mails que um admin cadastrou; login com Google ou GitHub e **2FA obrigatório em todo login** (TOTP ou passkey; o cadastro do 2FA acontece no primeiro login). A sessão do app vale **10 dias**.
- **Computadores e acesso**: cada computador é cadastrado por um usuário e acessado com **tíquetes** que o servidor emite (detalhes abaixo).
- **Relay iroh**: só aceita usuários logados e computadores cadastrados. Fica na mesma porta 443, em `/relay`.
- **HTTPS automático** com Let's Encrypt.
- Banco **SQLite** na pasta de dados.

## Requisitos

- Uma VPS Linux com IP público e um **domínio** apontando para ela (ex.: `rdesk.seu-dominio.com`). As passkeys exigem domínio, não funcionam com IP.
- Portas abertas: **443/tcp** (site, API e relay) e **7842/udp** (descoberta de endereço do relay, ajuda o hole punching).

## Instalação: binário + systemd

```sh
cargo build --release -p rdesk-server
sudo install -m 755 target/release/rdesk-server /usr/local/bin/
sudo useradd --system --home-dir /var/lib/rdesk-server --create-home rdesk
sudo -u rdesk rdesk-server init --public-url https://rdesk.seu-dominio.com --admin voce@gmail.com
sudo -u rdesk rdesk-server info     # mostra as URLs de retorno para cadastrar no Google/GitHub
sudo install -m 644 deploy/rdesk-server.service /etc/systemd/system/
sudo systemctl enable --now rdesk-server
journalctl -u rdesk-server -f
```

Os dados ficam em `/var/lib/rdesk-server` (`--data` ou `RDESK_SERVER_DATA` mudam isso): `server.toml`, `rdesk.db`, `server.key` e `acme/` (certificados).

## Instalação: Docker

```sh
docker compose -f deploy/docker-compose.yml build
docker compose -f deploy/docker-compose.yml run --rm rdesk-server \
  init --public-url https://rdesk.seu-dominio.com --admin voce@gmail.com
docker compose -f deploy/docker-compose.yml up -d
docker compose -f deploy/docker-compose.yml logs -f
```

Os dados ficam no volume `rdesk-data` (montado em `/data`). Os comandos de administração rodam do mesmo jeito: `docker compose -f deploy/docker-compose.yml run --rm rdesk-server users list`.

## Login com Google

1. Em [console.cloud.google.com](https://console.cloud.google.com/) → *APIs e serviços* → *Credenciais* → *Criar credenciais* → *ID do cliente OAuth* → tipo **Aplicativo da Web**.
2. *URIs de redirecionamento autorizados*: `https://rdesk.seu-dominio.com/login/oauth/google/callback`.
3. `rdesk-server oauth google --client-id … --client-secret …` e reinicie o servidor.

## Login com GitHub

1. GitHub → *Settings* → *Developer settings* → *OAuth Apps* → *New OAuth App*.
2. *Authorization callback URL*: `https://rdesk.seu-dominio.com/login/oauth/github/callback`.
3. `rdesk-server oauth github --client-id … --client-secret …` e reinicie o servidor.

O GitHub entra com qualquer e-mail **verificado** da conta que esteja na lista.

## Usuários

Pela VPS:

```sh
rdesk-server users list
rdesk-server users add maria@gmail.com [--admin]
rdesk-server users admin maria@gmail.com on|off
rdesk-server users disable maria@gmail.com on|off   # bloqueia, encerra as sessões e revoga os tíquetes
rdesk-server users remove maria@gmail.com
```

Ou pelo app, na aba **Admin** (só para admins do sistema), ou pela CLI do app: `rdesk users list|add|remove|admin|disable`. Sempre fica pelo menos um admin.

## Computadores e acesso

- Cada computador é cadastrado por um usuário (no app, aba *This computer*, ou `rdesk machines register`), com prova de posse da chave do host. Ele ganha uma credencial própria: usa o relay e sincroniza com o servidor mesmo sem ninguém logado.
- Quem pode ver: **só eu**, **compartilhado** (com pessoas e/ou grupos) ou **todos do servidor**.
- Cada compartilhamento tem um **papel**: *full control* (tela, teclado e mouse, clipboard, arquivos), *view only* (só a tela) ou *files only* (só arquivos). Quem tem vários compartilhamentos soma as permissões.
- **Co-gestores** (*Managers…*, só o dono escolhe): têm acesso total e podem compartilhar, renomear, mudar quem vê e ver o histórico. Descadastrar é só do dono.
- **Histórico** (*History…*): quem acessou, quando, por quanto tempo e com qual acesso. Visível para o dono, os co-gestores e os admins do sistema.
- Para conectar, o app pede um **tíquete** (válido por até 12 h); o host confere sozinho e aplica as permissões. Tirar ou reduzir o acesso revoga na hora e derruba a sessão em andamento.
- Admins do sistema veem todos os computadores (aba *Admin*), mas só acessam os compartilhados com eles.
- Os hosts não revelam nada antes do login (nome, sistema, se está em uso): o status vem do servidor.

## Manutenção

```sh
rdesk-server backup /root/rdesk-$(date +%F).db   # cópia consistente do banco, com o servidor rodando
rdesk-server rotate-key                         # nova chave dos tíquetes; reinicie o servidor
```

- **Backup**: guarde também o `server.toml` e o `server.key` (não estão no banco). Restaurar = parar o servidor, colocar os arquivos de volta, iniciar.
- **Rotação da chave**: depois do reinício os hosts recebem a chave nova no próximo sync (na hora) e continuam aceitando tíquetes da antiga por 12 h, então ninguém é desconectado. Não é preciso recadastrar nada.

## No app

- O app só abre a tela de login até a pessoa entrar: endereço do servidor → **Sign in with the browser** → Google/GitHub → 2FA → volta para o app.
- Linha de comando: `rdesk login rdesk.seu-dominio.com`, `rdesk whoami`, `rdesk logout`.

## Teste local (sem domínio)

```sh
rdesk-server --data ./srv init --public-url http://127.0.0.1:8080 --admin voce@exemplo.com --no-tls
# em ./srv/server.toml: dev_login = true   (login de teste digitando o e-mail; o 2FA continua obrigatório)
rdesk-server --data ./srv run
rdesk login http://127.0.0.1:8080
```

O `dev_login` só vale com `tls = "none"` e endereço local; em produção ele é ignorado.

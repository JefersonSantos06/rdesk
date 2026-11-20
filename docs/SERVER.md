# Servidor privado (`rdesk-server`)

> Passo a passo para montar o servidor do zero (domínio, portas, Google/GitHub, primeiro login): [Guia → Criar o servidor](guia/02-servidor.md). Esta página é a referência técnica.

Um binário só, na sua VPS:

- **Contas**: só e-mails que um admin cadastrou; login com Google ou GitHub e **2FA obrigatório em todo login** (TOTP ou passkey; o cadastro do 2FA acontece no primeiro login). A sessão do app vale **10 dias**.
- **Computadores e acesso**: cada computador é cadastrado por um usuário e acessado com **tíquetes** que o servidor emite (detalhes abaixo).
- **Relay iroh**: só aceita usuários logados e computadores cadastrados. Fica na mesma porta 443, em `/relay`.
- **HTTPS automático** com Let's Encrypt.
- Banco **SQLite** na pasta de dados.

## Requisitos

- Uma VPS Linux com IP público e um **domínio** apontando para ela (ex.: `rdesk.seu-dominio.com`). As passkeys exigem domínio, não funcionam com IP.
- Portas abertas: **443/tcp** (site, API e relay) e **7842/udp** (descoberta de endereço do relay, ajuda o hole punching).

## Instalação em um comando

```sh
curl -fsSL https://raw.githubusercontent.com/JefersonSantos06/rdesk/main/deploy/install-server.sh \
  | sudo bash -s -- --domain rdesk.seu-dominio.com --admin voce@gmail.com
```

Baixa a última versão (conferindo o SHA-256 do `release.json`), instala o programa e as unidades do systemd (inclusive a de atualização), cria o usuário `rdesk`, roda o `init`, abre 443/tcp e 7842/udp no `ufw` (se ativo) e mostra as URLs para o Google/GitHub. Atrás de um proxy que já faz HTTPS, acrescente `--listen 127.0.0.1:8090` (veja abaixo). Rodar de novo atualiza.

## Instalação: binário + systemd (à mão)

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

## Atrás de um proxy reverso (Caddy, nginx…)

Se as portas 80/443 já são de um proxy que faz o HTTPS, o `rdesk-server` roda sem TLS num endereço privado e o proxy repassa o domínio para ele (inclusive o `/relay`, que é um upgrade HTTP). No `server.toml`:

```toml
public_url = "https://rdesk.seu-dominio.com"
listen = "172.18.0.1:8090"      # um endereço que só o proxy alcança
tls = "none"
# O certificado do proxy para o mesmo domínio: a descoberta de endereço (QUIC, UDP 7842)
# do relay usa ele. Relido quando o proxy renova.
tls_cert = "/caminho/rdesk.seu-dominio.com.crt"
tls_key = "/caminho/rdesk.seu-dominio.com.key"
```

No Caddy:

```
rdesk.seu-dominio.com {
	reverse_proxy 172.18.0.1:8090
}
```

O DNS do domínio deve apontar direto para a VPS (na Cloudflare: *DNS only*, nuvem cinza), e a UDP 7842 precisa estar aberta.

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
rdesk-server users reset-2fa maria@gmail.com        # celular perdido: esquece o 2FA e encerra as sessões; ela cadastra outro
rdesk-server users remove maria@gmail.com
```

Ou pelo app, na aba **Admin** (só para admins do sistema), ou pela CLI do app: `rdesk users list|add|remove|admin|disable`. Sempre fica pelo menos um admin.

## Computadores e acesso

- Cada computador é cadastrado por um usuário, com prova de posse da chave do host. Ele ganha uma credencial própria: usa o relay e sincroniza com o servidor mesmo sem ninguém logado. Três formas:
  - logado nele: página *Este computador* ou `rdesk machines register`;
  - sem login nele, pelo **e-mail do dono**: o host pede para entrar (`rdesk host enroll --owner …` ou *Só compartilhar este computador* na tela de login) e o dono aprova no app, conferindo um código. Sem e-mail, os admins aprovam. Pedidos pendentes não entram no relay;
  - sem login nele, com uma **chave de cadastro** criada no app: entra na hora, já com a visibilidade e os grupos da chave. Ideal para imagens prontas (Raspberry Pi). Veja o [README](../README.md#computador-só-acessado-sem-login-nele).
- Quem pode ver: **só eu**, **compartilhado** (com pessoas e/ou grupos) ou **todos do servidor**.
- Cada compartilhamento tem um **papel**: *controle total* (tela, teclado e mouse, clipboard, arquivos, terminal), *só visualizar* (só a tela), *só arquivos* ou *só terminal*. Quem tem vários compartilhamentos soma as permissões.
- **Gestores** (em *Gerenciar acesso*, opção *controle total e pode gerenciar*; só o dono escolhe): têm acesso total e podem compartilhar, renomear, mudar quem vê e ver o histórico. Descadastrar é só do dono.
- **Quem tem acesso** (dono, gestores, pessoas e grupos) fica visível, só para leitura, para todos que podem acessar o computador.
- **No próprio computador**: o app prova que roda nele com uma assinatura da chave do host (cabeçalho `x-rdesk-here`, vale 5 minutos, ligada à conta logada). Quem estiver logado ali vê o computador no topo da lista, com o dono e com quem está compartilhado, mesmo que outra conta o tenha cadastrado. Também pode gerenciá-lo como um gestor (compartilhar, renomear, mudar quem vê, ver o histórico) e descadastrá-lo, depois que o sistema confirma um administrador: no Windows o app roda como administrador; no macOS e no Linux o sistema pede a senha de um administrador. A auditoria marca essas mudanças com *(at the computer)*. Com o serviço do Windows, a chave do host só é lida por administradores, e o serviço só assina "gerenciar" para administradores. Nos outros casos a chave é do usuário do sistema, e a confirmação é feita pelo app.
- **Histórico** (*Histórico completo*, no painel do computador): quem acessou, quando, por quanto tempo e com qual acesso. Visível para o dono, os gestores, quem gerencia no próprio computador e os admins do sistema.
- Para conectar, o app pede um **tíquete** (válido por até 12 h); o host confere sozinho e aplica as permissões. Tirar ou reduzir o acesso revoga na hora e derruba a sessão em andamento.
- Admins do sistema veem todos os computadores (aba *Admin*), mas só acessam os compartilhados com eles.
- Os hosts não revelam nada antes do login (nome, sistema, se está em uso): o status vem do servidor.

## Atualizar (versões novas)

O servidor olha os Releases do GitHub de hora em hora (`update_repo` no `server.toml`, vazio desliga) e guarda só as versões com o `release.json` assinado por uma chave de release do rdesk. No app, em **Admin**:

1. Em **Computadores**, marque uma ou mais máquinas como **Computador de teste**.
2. Em **Versões**, **Liberar para os de teste**: só elas recebem a versão no próximo sync (na hora).
3. Deu certo? **Liberar para todos…**: todas recebem.

Cada computador confere de novo a assinatura e o hash com as chaves embutidas nele, baixa do GitHub e instala quando ninguém está conectado:
- **Windows** (com o serviço): o próprio serviço; se a versão nova não sobe em 2 minutos, volta para a anterior (log em `C:\ProgramData\rdesk\logs\update.log`).
- **Linux** (`.deb`): o timer `rdesk-update` (desligado até configurar): `/etc/default/rdesk-update` com `RDESK_SERVER=rdesk.seu-dominio.com` (e `RDESK_MACHINE=<host ID>` para as máquinas de teste), depois `sudo systemctl enable --now rdesk-update.timer`.
- **Mac**: só avisa; baixe o novo `rdesk.app`.

Computadores não voltam de versão sozinhos: liberar uma versão mais antiga é recusado.

### O próprio servidor

**Atualizar o servidor…** (na mesma lista) baixa e confere a versão, faz backup do banco (`backups/`), troca o programa e reinicia; se a versão nova não responder, o programa e o banco anteriores voltam. Como o servidor roda sem privilégios, quem troca o programa é uma unidade root que roda o programa **já instalado** (que confere tudo de novo):

```sh
sudo install -m 644 deploy/rdesk-server-update.path deploy/rdesk-server-update.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now rdesk-server-update.path
```

As duas unidades vêm no `.tar.gz` do servidor. No Docker não há atualização por clique: atualize a imagem.

## Manutenção

```sh
rdesk-server backup /root/rdesk-$(date +%F).db   # cópia consistente do banco, com o servidor rodando
rdesk-server rotate-key                         # nova chave dos tíquetes; reinicie o servidor
```

- **Backup**: guarde também o `server.toml` e o `server.key` (não estão no banco). Restaurar = parar o servidor, colocar os arquivos de volta, iniciar.
- **Rotação da chave**: depois do reinício os hosts recebem a chave nova no próximo sync (na hora) e continuam aceitando tíquetes da antiga por 12 h, então ninguém é desconectado. Não é preciso recadastrar nada.

## No app

- O app só abre a tela de login até a pessoa entrar: endereço do servidor → **Entrar pelo navegador** → Google/GitHub → 2FA → volta para o app.
- Linha de comando: `rdesk login rdesk.seu-dominio.com`, `rdesk whoami`, `rdesk logout`.

## Teste local (sem domínio)

```sh
rdesk-server --data ./srv init --public-url http://127.0.0.1:8080 --admin voce@exemplo.com --no-tls
# em ./srv/server.toml: dev_login = true   (login de teste digitando o e-mail; o 2FA continua obrigatório)
rdesk-server --data ./srv run
rdesk login http://127.0.0.1:8080
```

O `dev_login` só vale com `tls = "none"` e endereço local; em produção ele é ignorado.

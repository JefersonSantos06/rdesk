# 2. Criar o servidor

O servidor é feito **uma vez só** e serve para todos os seus computadores e para as
pessoas que você liberar. Ele guarda as contas, a lista de computadores e é o **relay**
(a "ponte" quando dois computadores não conseguem se falar direto).

> Só quer usar dentro de casa, sem internet no meio? Pule este capítulo e vá para
> [Sem servidor](04-cenarios.md#f-sem-servidor-rede-local-ou-tailscale).

## O que você vai precisar

- [ ] Uma **VPS Linux** (Ubuntu ou Debian) com **IP público**, processador x86_64 ou ARM64
  e systemd. Não precisa ser grande: o servidor só repassa dados quando a ligação direta
  não dá certo.
- [ ] Um **domínio** (ou subdomínio) seu, por exemplo `rdesk.seu-dominio.com`. Precisa ser
  nome, não IP (o login com passkey exige).
- [ ] Uma conta **Google** ou **GitHub** para cada pessoa que vai entrar (é com ela que se
  faz o login).
- [ ] Um app de autenticação no celular (Google Authenticator, 1Password, Authy…) ou uma
  passkey, para o segundo fator.

Nos exemplos abaixo, troque `rdesk.seu-dominio.com` pelo seu domínio e
`voce@gmail.com` pelo seu e-mail.

## Passo 1: apontar o domínio para a VPS

No painel onde você comprou o domínio (Registro.br, Cloudflare, GoDaddy…), crie um
registro DNS:

| Tipo | Nome | Valor |
|---|---|---|
| `A` | `rdesk` (ou o subdomínio que escolheu) | o IP da VPS (ex.: `203.0.113.10`) |
| `AAAA` (se a VPS tiver IPv6) | `rdesk` | o IPv6 da VPS |

Na **Cloudflare**, deixe a nuvem **cinza** (*DNS only*). Com a nuvem laranja o relay não
funciona.

Confira (no seu computador): `ping rdesk.seu-dominio.com` deve mostrar o IP da VPS. Pode
levar alguns minutos para valer.

## Passo 2: abrir as portas

O servidor usa:

| Porta | Para quê |
|---|---|
| **443/tcp** | site, login, API e relay (HTTPS) |
| **7842/udp** | ajuda os computadores a acharem o caminho direto entre eles |

Se o provedor da VPS tem um firewall no painel (AWS, Oracle, Hetzner, Google Cloud…),
libere as duas lá. O firewall `ufw` da própria VPS o instalador abre sozinho.

## Passo 3: instalar

Entre na VPS pelo terminal (no Windows, o PowerShell serve):

```sh
ssh root@203.0.113.10
```

Cole este comando, trocando o domínio e o e-mail:

```sh
curl -fsSL https://raw.githubusercontent.com/JefersonSantos06/rdesk/main/deploy/install-server.sh | sudo bash -s -- --domain rdesk.seu-dominio.com --admin voce@gmail.com
```

O instalador:

1. baixa a última versão do servidor e confere que ela não foi alterada;
2. instala o programa em `/usr/local/bin/rdesk-server` e cria o usuário de sistema `rdesk`;
3. cria a configuração com **você como primeiro admin**;
4. abre as portas no `ufw` (se ele estiver ligado);
5. liga o servidor (e a atualização automática do próprio servidor) e mostra as
   **URLs de retorno** para o passo 4.

O certificado HTTPS (Let's Encrypt) é feito sozinho no primeiro acesso.

Confira: abra `https://rdesk.seu-dominio.com` no navegador. Deve aparecer uma página
simples "rdesk server" com um link de download.

## Passo 4: ligar o login com Google e/ou GitHub

As pessoas entram com a conta Google ou GitHub delas (e o e-mail precisa estar liberado).
Você precisa configurar **pelo menos um** dos dois. Os dois juntos também funcionam.

### Google

1. Abra [console.cloud.google.com](https://console.cloud.google.com/) e crie um projeto
   (ex.: "rdesk").
2. Menu → **APIs e serviços** → **Tela de permissão OAuth** (*OAuth consent screen*):
   - tipo **Externo**;
   - nome do app (ex.: "rdesk") e o seu e-mail de suporte;
   - salve. Enquanto o app estiver em **Teste**, só entram os e-mails listados em
     *Usuários de teste*: adicione lá cada pessoa, ou clique em **Publicar app**.
3. **Credenciais** → **Criar credenciais** → **ID do cliente OAuth**:
   - tipo de aplicativo: **Aplicativo da Web**;
   - em **URIs de redirecionamento autorizados**, adicione:
     `https://rdesk.seu-dominio.com/login/oauth/google/callback`
4. Copie o **ID do cliente** e a **chave secreta do cliente** e rode na VPS:

   ```sh
   sudo -u rdesk RDESK_SERVER_DATA=/var/lib/rdesk-server rdesk-server oauth google --client-id COLE_O_ID --client-secret COLE_A_CHAVE
   sudo systemctl restart rdesk-server
   ```

### GitHub

1. No GitHub: foto → **Settings** → **Developer settings** → **OAuth Apps** →
   **New OAuth App**.
2. Preencha:
   - *Application name*: rdesk
   - *Homepage URL*: `https://rdesk.seu-dominio.com`
   - *Authorization callback URL*: `https://rdesk.seu-dominio.com/login/oauth/github/callback`
3. **Register application** → **Generate a new client secret**. Copie o *Client ID* e o
   *client secret* e rode na VPS:

   ```sh
   sudo -u rdesk RDESK_SERVER_DATA=/var/lib/rdesk-server rdesk-server oauth github --client-id COLE_O_ID --client-secret COLE_A_CHAVE
   sudo systemctl restart rdesk-server
   ```

O GitHub aceita qualquer e-mail **verificado** da conta que esteja na lista de liberados.

Confira: `sudo -u rdesk RDESK_SERVER_DATA=/var/lib/rdesk-server rdesk-server info` mostra
`Sign-in: google, github` (ou o que você configurou).

## Passo 5: o primeiro login (e o segundo fator)

1. No seu computador, [instale o app](03-instalar-o-app.md) e abra.
2. Digite o endereço do servidor (`rdesk.seu-dominio.com`) e clique em
   **Entrar pelo navegador**.
3. O navegador abre: escolha **Continue with Google** ou **Continue with GitHub**.
4. Na primeira vez, o servidor pede para **proteger a conta** com um segundo fator. Ele é
   pedido em **todo** login. Escolha um:
   - **Create a passkey**: usa a biometria ou o PIN do computador/celular;
   - **Use an authenticator app (TOTP)**: escaneie o QR code com o app do celular (ou com o
     1Password) e digite o código de 6 dígitos.

   > **Guarde o segundo fator com cuidado** (por exemplo, o TOTP dentro do 1Password).
   > Perdeu o celular? Um admin pode **redefinir** o seu segundo fator e você cadastra um
   > novo no próximo login ([como](08-administrar.md#perdeu-o-segundo-fator)). Se você for o
   > único admin, guarde-o com ainda mais cuidado: a redefinição pela VPS também funciona
   > (`rdesk-server users reset-2fa`).
5. O navegador diz "You are signed in". Volte para o app: você está dentro, como admin.

O login vale **10 dias**. Depois o app pede para entrar de novo.

## Passo 6: liberar as outras pessoas

Só entram e-mails liberados. No app, em **Admin → Usuários → Liberar e-mail**, digite o
e-mail (marque *admin do sistema* só para quem vai administrar o servidor junto com você).
Detalhes em [Administrar](08-administrar.md).

Pronto: o servidor está no ar. Agora vá para os [Cenários de uso](04-cenarios.md).

---

## Opcional: atrás de um proxy reverso (Caddy, nginx…)

Se a VPS já tem um site nas portas 80/443 com um proxy que faz o HTTPS, o rdesk-server roda
**sem HTTPS** num endereço interno e o proxy repassa o domínio para ele.

1. Instale com `--listen` (um endereço que só o proxy alcança):

   ```sh
   curl -fsSL https://raw.githubusercontent.com/JefersonSantos06/rdesk/main/deploy/install-server.sh | sudo bash -s -- --domain rdesk.seu-dominio.com --admin voce@gmail.com --listen 127.0.0.1:8090
   ```

2. No Caddy (`/etc/caddy/Caddyfile`):

   ```
   rdesk.seu-dominio.com {
   	reverse_proxy 127.0.0.1:8090
   }
   ```

   e `sudo systemctl reload caddy`. (Se o Caddy roda em Docker, use um endereço que o
   container alcance, como o IP da rede Docker, em vez de `127.0.0.1`.)
3. A porta **7842/udp** continua sendo do rdesk-server e precisa do certificado do domínio.
   Em `/var/lib/rdesk-server/server.toml`, aponte para o certificado que o proxy já tem
   (o usuário `rdesk` precisa conseguir ler os arquivos):

   ```toml
   tls_cert = "/caminho/rdesk.seu-dominio.com.crt"
   tls_key = "/caminho/rdesk.seu-dominio.com.key"
   ```

   No Caddy instalado pelo pacote, eles ficam em
   `/var/lib/caddy/.local/share/caddy/certificates/acme-v02.api.letsencrypt.org-directory/rdesk.seu-dominio.com/`.
   O rdesk relê os arquivos quando o proxy renova. Depois: `sudo systemctl restart rdesk-server`.

Sem o passo 3, tudo funciona, mas os computadores acham o caminho direto entre eles com
menos frequência (e usam mais o relay).

## Opcional: com Docker

```sh
git clone https://github.com/JefersonSantos06/rdesk.git && cd rdesk
docker compose -f deploy/docker-compose.yml build
docker compose -f deploy/docker-compose.yml run --rm rdesk-server init --public-url https://rdesk.seu-dominio.com --admin voce@gmail.com
docker compose -f deploy/docker-compose.yml up -d
```

Os comandos de administração rodam do mesmo jeito, por exemplo
`docker compose -f deploy/docker-compose.yml run --rm rdesk-server oauth google --client-id … --client-secret …`.
No Docker não há atualização por clique: atualize a imagem.

## Opcional: testar sem domínio (no próprio computador)

Só para experimentar, sem Google/GitHub:

```sh
rdesk-server --data ./srv init --public-url http://127.0.0.1:8080 --admin voce@exemplo.com --no-tls
```

Em `./srv/server.toml`, acrescente `dev_login = true` (login digitando o e-mail; o segundo
fator continua obrigatório) e rode:

```sh
rdesk-server --data ./srv run
```

No app, use o servidor `http://127.0.0.1:8080`. O `dev_login` é ignorado fora desse modo
local.

## Onde ficam as coisas no servidor

| O quê | Onde |
|---|---|
| Programa | `/usr/local/bin/rdesk-server` |
| Dados (configuração, banco, chaves, certificados) | `/var/lib/rdesk-server/` (`server.toml`, `rdesk.db`, `server.key`, `acme/`) |
| Logs | `journalctl -u rdesk-server -f` |
| Ligar, desligar, reiniciar | `sudo systemctl start\|stop\|restart rdesk-server` |

Backup, atualizações e outros cuidados: [Administrar](08-administrar.md).

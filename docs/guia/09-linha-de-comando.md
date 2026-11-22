# 9. Linha de comando

Tudo o que o app faz também existe como comando. É útil para automatizar, para computadores
sem tela e para quem prefere o terminal. As mensagens da linha de comando são em inglês.

## Onde está o programa

| Sistema | Comando |
|---|---|
| Linux (pacote) | `rdesk` |
| Windows (instalado) | `& "C:\Program Files\rdesk\rdesk.exe"` no PowerShell (ou entre na pasta e use `.\rdesk.exe`) |
| Windows (sem instalar) | `.\rdesk@rdesk.seu-dominio.com.exe`, na pasta do download |
| Mac | `/Applications/rdesk.app/Contents/MacOS/rdesk` |

Nos exemplos abaixo, troque `rdesk` pelo caminho do seu sistema. `rdesk --help` e
`rdesk <comando> --help` mostram todas as opções.

## Conta

```sh
rdesk login rdesk.seu-dominio.com   # abre o navegador: Google/GitHub + segundo fator
rdesk whoami                        # quem está logado, até quando, e se o relay responde
rdesk logout                        # sai do servidor em uso
```

Com mais de um servidor (veja [Mais de um servidor](05-usar.md#mais-de-um-servidor)), cada
`rdesk login` adiciona uma conta e passa a usá-la; as outras continuam logadas.

```sh
rdesk server list                   # as contas salvas; * marca a em uso
rdesk server use 2                  # usar outra: o número da lista, o servidor, o nome dele ou o e-mail
rdesk machines list --account rdesk.empresa-x.com.br   # só neste comando, sem trocar a em uso
```

`--account` vale para qualquer comando (`connect`, `send`, `shell`…).

## Abrir o app

```sh
rdesk                       # abre a janela (o mesmo que dois cliques)
rdesk ui --minimized        # abre escondido na bandeja
rdesk autostart login       # abrir junto com o sistema (off para desligar; no Windows também login-admin)
```

## Conectar e transferir

`<computador>` pode ser o **nome** no servidor, o **ID do host**, ou `ID@endereço` para um
computador da rede local.

```sh
rdesk connect "PC da sala"              # abre a janela da sessão
rdesk connect "PC da sala" --files      # já no painel de arquivos
rdesk connect "PC da sala" --terminal   # já com um terminal
rdesk connect <ID>@192.168.0.20         # rede local, sem servidor

rdesk send "PC da sala" relatorio.pdf fotos.zip              # envia para Downloads/rdesk de lá
rdesk ls "PC da sala" "~/Documents"                          # lista uma pasta de lá
rdesk get "PC da sala" "~/Documents/planilha.xlsx" --out .   # baixa um arquivo
rdesk shell "PC da sala"                 # um terminal de lá, neste terminal
rdesk shell "PC da sala" --admin         # como administrador (Windows, controle total)
rdesk tunnel "PC da sala" -L 13389:localhost:3389   # a porta 13389 daqui leva à 3389 de lá (até Ctrl+C)
rdesk machines wake "PC da sala"         # ligar pela rede (Wake-on-LAN, por outro computador ligado lá)

rdesk connect "PC da sala" --headless --frames 300   # teste de conexão: mostra latência, fps, velocidade
```

Opções de login (para computadores com senha/chave própria): `--auth agent|password`,
`--key SHA256:…` (qual chave do agente), `--password-ref op://…` (senha do 1Password),
`--identity arquivo` (chave SSH em arquivo). `rdesk agent-keys` lista as chaves do agente
SSH (1Password).

## Computadores no servidor

```sh
rdesk machines list                                         # os que você acessa (e os seus)
rdesk machines register --name "PC da sala" --visibility private   # cadastra ESTE computador
rdesk machines unregister                                   # descadastra este computador
rdesk machines rename "PC da sala" "PC da sala 2"
rdesk machines visibility "PC da sala" private|shared|everyone
rdesk machines shares "PC da sala"                          # com quem está compartilhado
rdesk machines share "PC da sala" --user maria@gmail.com:view --group Família
#   níveis: control (padrão), view, files, terminal. Substitui a lista e muda para "shared".
rdesk machines managers "PC da sala"                        # mostra os gestores
rdesk machines managers "PC da sala" --set joao@gmail.com   # define (só o dono; --set vazio limpa)
rdesk machines history "PC da sala"
```

### Aprovações e chaves de cadastro

```sh
rdesk machines requests                                   # computadores esperando a sua aprovação
rdesk machines approve 3F2A-91C0 --name "Pi da sala" --visibility shared
rdesk machines refuse 3F2A-91C0
rdesk machines keys create "Raspberry Pis" --days 30 --group Família:view   # mostra a chave uma vez
rdesk machines keys create "PC da vó" --days 1 --once
rdesk machines keys list
rdesk machines keys revoke "Raspberry Pis"
```

## Grupos

```sh
rdesk groups list
rdesk groups create Família
rdesk groups add Família joao@gmail.com [--admin]
rdesk groups role Família joao@gmail.com admin|member
rdesk groups remove Família joao@gmail.com
rdesk groups rename Família "Família Silva"
rdesk groups leave Família
rdesk groups delete Família
```

## Este computador (host)

```sh
rdesk host id                    # mostra o ID do host
rdesk host run                   # roda o host (em primeiro plano; Ctrl+C para)
rdesk host run --port 7420       # com outra porta UDP
rdesk host check                 # diagnóstico: permissões, monitores, rede

rdesk host set-password                                   # pergunta a senha
rdesk host set-password --op "op://Private/rdesk/password"   # lê do 1Password
rdesk host clear-password
rdesk host add-key --from-agent                           # escolhe uma chave do agente SSH
rdesk host add-key "ssh-ed25519 AAAA… fulano"             # ou cola a chave pública
rdesk host list-keys
rdesk host remove-key SHA256:…
```

### Entrar num servidor sem conta (computador só acessado)

```sh
rdesk host enroll --server rdesk.seu-dominio.com --key rdk_…            # entra na hora
rdesk host enroll --server rdesk.seu-dominio.com --owner voce@gmail.com # o dono aprova
rdesk host enroll --name "PC da vó" …                                   # nome no servidor
rdesk host enroll --clear                                               # desiste do pedido
rdesk host run                                                          # o pedido sai daqui
```

As variáveis `RDESK_SERVER`, `RDESK_OWNER`, `RDESK_ENROLL_KEY` e `RDESK_NAME` fazem o mesmo
que o `enroll`.

## Windows: instalação e serviço

```powershell
rdesk install                                     # instala tudo (programa, serviço, menu, atualizações)
rdesk install --only-share --key rdk_…            # computador só acessado, com chave
rdesk install --only-share --owner voce@gmail.com --name "PC da vó"   # com aprovação
rdesk uninstall                                   # desinstala (o ID do host é mantido)
rdesk service install | uninstall | status        # só o serviço (terminal de administrador)
```

O servidor sai do nome do arquivo (`rdesk@servidor.exe`) ou de `--server`.

## Linux: atualizar

```sh
sudo rdesk update --server rdesk.seu-dominio.com           # instala a versão liberada
sudo rdesk update --server rdesk.seu-dominio.com --check   # só diz se há algo novo
```

Normalmente o `rdesk-update.timer` faz isso sozinho a cada hora.

## Admin (pelo app)

```sh
rdesk users list
rdesk users add maria@gmail.com [--admin]
rdesk users admin maria@gmail.com on|off
rdesk users disable maria@gmail.com on|off
rdesk users reset-2fa maria@gmail.com     # celular perdido: cadastra um segundo fator novo no próximo login
rdesk users remove maria@gmail.com
```

## Servidor (`rdesk-server`)

Na VPS, os comandos rodam como o usuário `rdesk`, apontando para a pasta de dados:

```sh
sudo -u rdesk RDESK_SERVER_DATA=/var/lib/rdesk-server rdesk-server <comando>
```

| Comando | Para quê |
|---|---|
| `init --public-url https://dominio --admin email [--no-tls] [--listen endereço]` | cria a configuração e o primeiro admin (o instalador já faz) |
| `run` | roda o servidor (o systemd já faz) |
| `info` | mostra a configuração, o que está ligado e as URLs de retorno para o Google/GitHub |
| `oauth google --client-id … --client-secret …` | liga o login com Google (o mesmo com `github`); reinicie depois |
| `users list` / `users add email [--admin]` / `users admin email on\|off` / `users disable email on\|off` / `users remove email` | quem pode entrar |
| `users reset-2fa email` | esquece o segundo fator da pessoa (celular perdido) e encerra as sessões dela; ela cadastra outro no próximo login |
| `backup arquivo.db` | cópia segura do banco, com o servidor rodando |
| `rotate-key` | nova chave dos tíquetes; reinicie depois |

`--data pasta` (antes do comando) troca a pasta de dados, como a variável
`RDESK_SERVER_DATA`.

## Arquivos de configuração

Normalmente você não precisa mexer neles: o app cuida de tudo.

| O quê | Windows | Mac | Linux |
|---|---|---|---|
| Configurações do app (`client.toml`), logins (`accounts.json`), logs | `%APPDATA%\rdesk\rdesk\config\` | `~/Library/Application Support/dev.rdesk.rdesk/` | `~/.config/rdesk/` |
| Host (`host.toml`, identidade, senha, chaves) | a mesma pasta; com o serviço: `C:\ProgramData\rdesk\` | a mesma pasta | a mesma pasta |

O `host.toml`, para quem quiser ajustar à mão:

```toml
port = 7420              # porta UDP para conexões diretas
max_fps = 30
jpeg_quality = 70
clipboard = true         # área de transferência (texto)
require_local = false    # cadeado extra
admin_terminal = true    # Windows: terminal como administrador permitido
# inbox_dir = "C:/Users/voce/Downloads/rdesk"   # onde chegam os arquivos

[direct]
# enabled = true         # padrão: ligado sem servidor, desligado depois de cadastrado
from = ["lan", "tailscale"]
mdns = false             # anunciar na rede local
```

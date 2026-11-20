# 8. Administrar

Este capítulo é para os **admins do sistema** (quem instalou o servidor e quem foi
promovido). No app, eles veem a página **Admin** na barra lateral.

## Quem pode entrar (Admin → Usuários)

Só entram e-mails liberados.

- **Liberar e-mail**: digite o e-mail, marque **admin do sistema** se for o caso, e
  **Liberar**. A pessoa já pode entrar com o Google ou GitHub desse e-mail.
- No **⋯** de cada pessoa:
  - **Tornar admin** / **Tornar usuário**;
  - **Desativar** / **Ativar**: desativada, a pessoa não entra, as sessões dela terminam e
    os acessos dela são revogados na hora;
  - **Remover…**: tira da lista (para desfazer, libere o e-mail de novo).
- Os selos mostram se a pessoa já configurou o segundo fator (*TOTP*, *passkey*) e quando
  entrou pela última vez.

Sempre fica pelo menos um admin. O mesmo pela VPS: `rdesk-server users …` (veja
[Linha de comando](09-linha-de-comando.md#servidor-rdesk-server)).

### Perdeu o segundo fator

Quando alguém perde o celular com o app autenticador (ou quer trocar de passkey):

1. **Confirme que é mesmo a pessoa** pedindo (por telefone, pessoalmente). Quem redefine
   abre a porta para quem tiver a conta Google/GitHub dela.
2. **Admin → Usuários** → **⋯** da pessoa → **Redefinir segundo fator…** → **Redefinir**.
3. O app autenticador e as passkeys dela deixam de valer, e ela sai da conta em todos os
   lugares. No próximo login, o servidor pede para cadastrar um segundo fator novo.

Pela VPS (útil se **você**, o único admin, perdeu o seu):

```sh
sudo -u rdesk RDESK_SERVER_DATA=/var/lib/rdesk-server rdesk-server users reset-2fa voce@gmail.com
```

## Todos os computadores (Admin → Computadores)

A lista de **todos** os computadores do servidor, com dono, visibilidade, situação e versão.
Admins **veem** todos, mas só **acessam** os que foram compartilhados com eles.

A chave **Computador de teste** marca quem recebe as versões novas primeiro (abaixo).

## Atualizações (Admin → Versões)

As versões novas do rdesk aparecem sozinhas: o servidor olha o GitHub de hora em hora (ou
clique em **Verificar agora**) e só aceita versões assinadas pelo projeto.

Nenhum computador se atualiza sem você liberar. O caminho seguro:

1. Em **Admin → Computadores**, ligue **Computador de teste** em um ou dois computadores.
2. Em **Admin → Versões**, na versão nova, **Liberar para os de teste**. Eles recebem na
   hora e instalam quando ninguém estiver conectado neles.
3. Funcionou? **Liberar para todos…**. Todos recebem.

Não dá para voltar a uma versão mais antiga liberando-a de novo.

Como cada sistema se atualiza:

| Sistema | Como |
|---|---|
| **Windows** (instalado, com o serviço) | o serviço baixa, confere e instala sozinho quando ninguém está conectado. Se a versão nova não subir em 2 minutos, volta para a anterior. |
| **Linux** (instalado pelo `install.sh`) | verifica a cada hora e instala o pacote novo. O host reinicia quando ninguém está conectado. |
| **Mac** | se o rdesk.app está numa pasta em que dá para gravar (ex.: Aplicativos) e o Mac está **cadastrado**, ele se troca sozinho quando ninguém está conectado. Senão, baixe o novo na página de download. |

A página `https://rdesk.seu-dominio.com/download` sempre oferece a versão liberada para
todos.

> **Linux instalado à mão** (sem o `install.sh`): para ligar as atualizações, crie
> `/etc/default/rdesk-update` com a linha `RDESK_SERVER=rdesk.seu-dominio.com` (e, num
> computador de teste, `RDESK_MACHINE=<ID do host>`) e rode
> `sudo systemctl enable --now rdesk-update.timer`.

### Atualizar o próprio servidor

Na mesma lista de versões, **Atualizar o servidor…**: ele faz backup do banco, troca o
programa e reinicia. Se a versão nova não responder, volta tudo como estava.

Ou, pela VPS, rode de novo o instalador (ele atualiza):

```sh
curl -fsSL https://raw.githubusercontent.com/JefersonSantos06/rdesk/main/deploy/install-server.sh | sudo bash
```

## Backup do servidor

O que importa está em `/var/lib/rdesk-server/`. Para uma cópia segura do banco, mesmo com
o servidor rodando:

```sh
sudo -u rdesk RDESK_SERVER_DATA=/var/lib/rdesk-server rdesk-server backup /var/lib/rdesk-server/backup-$(date +%F).db
```

Guarde também `server.toml` e `server.key` (não estão no banco). Copie tudo para fora da
VPS (seu computador, outro serviço).

**Restaurar** (numa VPS nova): instale o servidor com o mesmo domínio, pare-o
(`sudo systemctl stop rdesk-server`), coloque de volta `rdesk.db`, `server.toml` e
`server.key` em `/var/lib/rdesk-server/` (dono: usuário `rdesk`) e inicie
(`sudo systemctl start rdesk-server`). Os computadores cadastrados continuam funcionando,
sem recadastrar.

## Trocar a chave dos tíquetes

Se achar que a chave do servidor vazou:

```sh
sudo -u rdesk RDESK_SERVER_DATA=/var/lib/rdesk-server rdesk-server rotate-key
sudo systemctl restart rdesk-server
```

Os computadores recebem a chave nova em segundos e aceitam a antiga por mais 12 horas:
ninguém é desconectado e nada precisa ser recadastrado.

## Logs do servidor

```sh
journalctl -u rdesk-server -f        # ao vivo
journalctl -u rdesk-server -n 200    # as últimas 200 linhas
sudo -u rdesk RDESK_SERVER_DATA=/var/lib/rdesk-server rdesk-server info   # configuração e URLs
```

## Configuração do servidor

Tudo fica em `/var/lib/rdesk-server/server.toml` (por exemplo, o nome mostrado na página de
login). Edite e reinicie (`sudo systemctl restart rdesk-server`).

**Evite mudar o domínio** depois de tudo montado: os computadores cadastrados e o login de
todo mundo apontam para ele, e as URLs de retorno no Google/GitHub também.

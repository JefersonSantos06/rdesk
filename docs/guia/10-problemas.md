# 10. Problemas e soluções

Procure o sintoma. Se nada resolver, veja [Onde estão os logs](#onde-estão-os-logs) e o
[Diagnóstico](#diagnóstico).

## Entrar (login)

**"… não está liberado neste servidor"**
Um admin precisa liberar o seu e-mail (*Admin → Usuários → Liberar e-mail*). No GitHub,
vale qualquer e-mail **verificado** da conta.

**O Google diz "Acesso bloqueado" ou "app em teste"**
No Google Cloud, em *Tela de permissão OAuth*, adicione o e-mail em **Usuários de teste**
ou clique em **Publicar app** ([passo 4 do servidor](02-servidor.md#google)).

**`redirect_uri_mismatch` (Google) ou "redirect URI" (GitHub)**
A URL de retorno cadastrada está diferente. Ela deve ser exatamente
`https://rdesk.seu-dominio.com/login/oauth/google/callback` (ou `/github/callback`).
`rdesk-server info` mostra as URLs certas.

**O navegador diz que o site não é seguro, ou não abre**
- O domínio aponta para o IP da VPS? (`ping rdesk.seu-dominio.com`)
- A porta **443/tcp** está aberta no firewall do provedor?
- Na Cloudflare, a nuvem está **cinza**?
- Logs: `journalctl -u rdesk-server -n 100`.

**Perdi o celular com o segundo fator**
Peça a um admin para **redefinir o segundo fator** (*Admin → Usuários → ⋯ → Redefinir
segundo fator…*). No próximo login você cadastra um novo. Se você é o único admin, faça pela
VPS: `rdesk-server users reset-2fa seu@email` ([como](08-administrar.md#perdeu-o-segundo-fator)).

**"Your sign-in expired"**
O login vale 10 dias. Entre de novo.

## O computador aparece "Offline"

Confira **no computador que deveria estar online**:

1. O app está aberto (ou na bandeja), ou o serviço do Windows está instalado?
2. Em **Este computador**, o interruptor está **Ligado**? A situação diz *Pronto para acesso
   remoto*?
3. Ele está **cadastrado**? (*Configuração do acesso* mostra "Cadastrado como …")
4. Ele tem internet? Não está **dormindo**?
5. **Windows com serviço**: `rdesk service status` diz que está rodando?
6. **Linux só acessado**: `systemctl --user status rdesk-host`. Precisa da sessão gráfica
   (X11) aberta: configure o login automático.

## Conecta, mas…

**…a tela fica preta, ou "nenhum monitor"**
- **Mac**: falta a permissão **Gravação de Tela**. *Este computador* mostra um aviso com o
  botão **Abrir Gravação de Tela** ([detalhes](03-instalar-o-app.md#mac)).
- **Linux com Wayland**: alguém no computador precisa clicar em **permitir** na janela do
  sistema que pede para compartilhar a tela. Para usar o X11 em vez do portal, inicie com a
  variável `RDESK_WAYLAND=0` (ou escolha a sessão "Xorg" no login).
- **Windows**: a tela de bloqueio e as janelas de administrador (UAC) só aparecem com o
  **serviço** instalado.
- Tente *Monitor → Atualizar imagem* na barra.

**…vejo a tela mas o mouse e o teclado não funcionam**
- O seu acesso pode ser **só visualizar** (o painel *Info* mostra o nível).
- **Mac**: falta a permissão **Acessibilidade** (botão **Abrir Acessibilidade** em *Este
  computador*).
- **Windows**: janelas de administrador (Gerenciador de Tarefas, instaladores, UAC)
  ignoram cliques sem o **serviço** instalado. O app avisa com uma mensagem no canto.
- **Wayland**: o sistema pode ter liberado só a tela; reconecte e permita também teclado e
  mouse.

**…Ctrl+Alt+Del não faz nada**
Só funciona em computadores Windows com o **serviço** instalado (e pelo menu *Teclas*).

**…está lento ou travando**
- Veja **Info → Rota**. Se for **relay**, veja o [cenário H](04-cenarios.md#h-quero-a-ligação-direta-sem-passar-pelo-relay).
- Em **Qualidade**, escolha **Velocidade**.
- Internet fraca de um dos lados limita tudo: o rdesk baixa o fps em vez de atrasar a
  imagem.

**…a sessão caiu com "outro cliente conectou"**
Só cabe uma sessão por computador. Alguém (ou você, de outro lugar) conectou depois.

**…a sessão caiu com "encerrada pelo computador"**
Quem está na frente do computador clicou em **Encerrar** (ou em *Desconectar*), ou o seu
acesso foi retirado.

## Rede local (sem servidor)

**Não conecta pelo IP**
1. O endereço está certo? Ele muda se o roteador der outro IP: reserve um IP fixo.
2. Os dois estão na **mesma rede**? De fora da rede local (ou do Tailscale) a conexão é
   recusada de propósito.
3. **Windows**: permita o rdesk no firewall para **redes privadas** (ou instale o serviço,
   que cria a regra). Confira também se a rede do Windows está como *Privada*, não
   *Pública*.
4. No computador acessado, **Este computador → Segurança → Acesso direto** está ligado, com
   **rede local** marcado? Num computador **cadastrado** ele vem desligado.
5. Tem senha ou chave definida lá? Sem elas, o host não liga.

**Não acha pelo ID (sem endereço)**
O outro computador precisa ter **Anunciá-lo na rede local (mDNS)** ligado, e os dois
precisam estar na mesma rede (sem isolamento de clientes no Wi-Fi). Na dúvida, use o
endereço.

**"UDP port 7420 is already in use"**
Já há um host rdesk rodando neste computador (por exemplo, o app e o serviço, ou duas
cópias). Feche um, ou troque a porta em *Este computador → Desempenho e arquivos*.

## Arquivos e terminal

**Enviar arquivo é recusado**
- O seu acesso precisa incluir arquivos (*controle total* ou *só arquivos*).
- Com o serviço do Windows, precisa haver alguém logado no Windows (os arquivos vão para os
  Downloads dessa pessoa).

**"Terminal (admin)" é recusado**
Precisa de: computador **Windows**, **controle total**, o **serviço** instalado lá, a pessoa
logada ser administradora, e a opção ligada em *Este computador → Desempenho e arquivos*.

## Instalação

**O Windows bloqueia o programa (SmartScreen)**
**Mais informações → Executar assim mesmo**.

**O antivírus apaga ou bloqueia o rdesk**
Crie uma exceção para `C:\Program Files\rdesk\rdesk.exe` (ou para o arquivo baixado).
Programas de acesso remoto são parecidos com vírus de acesso remoto aos olhos do antivírus.

**Mac: "não pode ser aberto porque é de um desenvolvedor não identificado"**
Botão direito no rdesk.app → **Abrir** → **Abrir**. Ou, no Terminal:
`xattr -dr com.apple.quarantine /Applications/rdesk.app`.

**Linux: não aparece no menu de aplicativos**
Até a versão 0.5.0 o pacote não criava o ícone: atualize, ou abra com `rdesk` no terminal.
Para abrir sozinho, ligue *Configurações → Geral → Iniciar com o sistema*.

**Linux: `install.sh` diz "--host: run it with sudo from the desktop user's account"**
Rode o comando logado como o usuário da área de trabalho (com `sudo`), não como root.

## Diagnóstico

No computador com problema: **Este computador → Diagnóstico → Verificar**. Ou no terminal:

```sh
rdesk host check     # permissões, monitores, rede, sessão (X11/Wayland), administrador
rdesk whoami         # login, validade e se o relay responde
```

E, para medir uma conexão sem abrir janela:

```sh
rdesk connect "Nome do computador" --headless --frames 300
```

## Onde estão os logs

| Log | Windows | Mac | Linux |
|---|---|---|---|
| App | `%APPDATA%\rdesk\rdesk\config\logs\app.log` | `~/Library/Application Support/dev.rdesk.rdesk/logs/app.log` | `~/.config/rdesk/logs/app.log` |
| Cada sessão aberta | `…\logs\session-<nome>.log` (mesma pasta) | idem | idem |
| Serviço do Windows | `C:\ProgramData\rdesk\logs\service.log`, `host.log`, `update.log` | — | — |
| Host como serviço (Linux) | — | — | `journalctl --user -u rdesk-host` |
| Atualizações (Linux) | — | — | `journalctl -u rdesk-update` |
| Servidor | — | — | `journalctl -u rdesk-server` (na VPS) |

O caminho certo da sua pasta aparece em *Configurações → Avançado* (com o botão **Abrir**).
Logs muito grandes são apagados quando o programa abre.

Para mais detalhes nos logs, rode o comando com `-v` (ou `-vv`), por exemplo
`rdesk -v host run`.

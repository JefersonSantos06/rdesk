# 4. Cenários de uso

Cada cenário é uma receita completa. Ache o seu, siga os passos na ordem.

- [A. Usar e ser acessado (cliente + host)](#a-usar-e-ser-acessado-cliente--host)
- [B. Só acessar outros (só cliente)](#b-só-acessar-outros-só-cliente)
- [C. Só ser acessado, com a sua conta](#c-só-ser-acessado-com-a-sua-conta)
- [D. Só ser acessado, sem conta nele](#d-só-ser-acessado-sem-conta-nele)
- [E. Vários computadores de uma vez (chave de cadastro)](#e-vários-computadores-de-uma-vez-chave-de-cadastro)
- [F. Sem servidor (rede local ou Tailscale)](#f-sem-servidor-rede-local-ou-tailscale)
- [G. Servidor e rede local juntos (acesso direto)](#g-servidor-e-rede-local-juntos-acesso-direto)
- [H. Quero a ligação direta, sem passar pelo relay](#h-quero-a-ligação-direta-sem-passar-pelo-relay)

Os cenários A a E e G precisam de um [servidor](02-servidor.md) e do seu e-mail liberado
nele. O F não precisa de nada além do app.

---

## A. Usar e ser acessado (cliente + host)

*Exemplo: o seu PC e o seu notebook, cada um acessando o outro.*

Faça isto **em cada computador**:

1. [Instale o app](03-instalar-o-app.md) e entre com a sua conta.
2. Vá em **Este computador**. No cartão do topo, ligue o **interruptor** (fica
   *Ligado*). O app lembra a escolha.
3. Na lista **Configuração do acesso**, clique em **Cadastrar…**:
   - **Nome**: como ele vai aparecer na lista (ex.: "PC do escritório");
   - **Quem pode acessar**: **Só eu** (você pode mudar depois);
   - **Cadastrar**.
4. Para ele continuar acessível quando você não estiver com o app aberto:
   - **Windows**: na mesma lista, **Instalar** (o serviço do Windows). Assim ele funciona até
     na tela de bloqueio e antes de você entrar no Windows.
   - **Mac**: dê as permissões de **Gravação de Tela** e **Acessibilidade**
     ([como](03-instalar-o-app.md#mac)) e ligue *Configurações → Geral → Iniciar com o
     sistema*.
   - **Linux**: use a sessão **X11** e ligue *Configurações → Geral → Iniciar com o sistema*.

**Testar**: no outro computador, abra **Computadores**. O computador aparece com
**Online**. Selecione e clique em **Conectar** (ou dê dois cliques). Uma janela abre com a
tela dele.

> Um computador **cadastrado** tem a própria credencial no servidor: ele continua
> acessível mesmo que você saia da conta no app dele (desde que o host esteja rodando).

---

## B. Só acessar outros (só cliente)

*Exemplo: o notebook do trabalho, que nunca deve ser acessado.*

1. [Instale o app](03-instalar-o-app.md) e entre com a sua conta.
2. Pronto. Em **Este computador**, deixe o interruptor **Desligado** (ele já vem assim) e
   **não** cadastre.
3. Para conectar: **Computadores** → selecione → **Conectar**.

Se quiser o mínimo de coisas instaladas no Windows, nem precisa clicar em *Instalar*: o
`.exe` baixado funciona sozinho. (Sem instalar, ele não se atualiza sozinho.)

---

## C. Só ser acessado, com a sua conta

*Exemplo: o PC de casa, que você acessa do trabalho.*

É o cenário A sem usar o cliente. Siga os passos 1 a 4 do [cenário A](#a-usar-e-ser-acessado-cliente--host)
neste computador e mais:

- **Não deixe o computador dormir**:
  - Windows: *Configurações → Sistema → Energia → Tela e suspensão*: "Colocar o
    dispositivo em suspensão" → **Nunca** (quando ligado na tomada).
  - Mac: *Ajustes do Sistema → Bateria* (ou *Economia de Energia*) → **Impedir repouso
    automático** com a tela desligada.
  - Linux: nas configurações de energia, desligue a suspensão automática.
- **Windows**: o serviço (passo 4) é o que garante o acesso depois de um reinício, antes
  de alguém entrar no Windows.
- **Linux sem ninguém usando** (um servidor, um PC na estante): o host pode rodar como
  serviço do seu usuário, sem o app aberto. Depois de cadastrar pelo app (passo 3):

  ```sh
  sudo loginctl enable-linger $USER
  systemctl --user enable --now rdesk-host
  ```

  Configure também o **login automático** na sessão X11, para haver uma tela para
  mostrar. E **desligue o interruptor do host no app** (ou não deixe o app abrir com o
  sistema): só pode haver um host rodando por computador.

---

## D. Só ser acessado, sem conta nele

*Exemplo: o PC da vó, de um cliente, de um amigo. Ninguém lá vai entrar com conta: o
computador pede para entrar no seu servidor e passa a ser **seu**.*

Há dois jeitos de autorizar a entrada:

| Jeito | Como funciona | Melhor para |
|---|---|---|
| **Chave de cadastro** (recomendado) | Você cria um código `rdk_…` e usa na instalação. O computador entra **na hora**. | Quase sempre: é o mais simples. |
| **E-mail do dono** | O computador pede para entrar com o seu e-mail; você **aprova** no seu app, conferindo um código. | Quando você não quer passar um código adiante. |

### D1. Criar a chave de cadastro (no SEU computador)

1. No seu app: **Computadores** → **＋ Adicionar** → **Vários computadores (chave de
   cadastro)** → **Nova chave…**.
2. Preencha:
   - **Nome**: para você lembrar (ex.: "PC da vó");
   - **Uso**: *um computador* (a chave só vale uma vez);
   - **Válida por**: 1 dia é suficiente;
   - **Quem pode ver os computadores que ela adiciona**: quem vai poder acessá-lo (*só eu*,
     ou compartilhado com um grupo).
3. **Criar chave**. A chave aparece **uma vez só**: copie (ela começa com `rdk_`).

### D2. No computador que vai ser acessado

Em Windows e Linux, use o **`rdesk-host`**: só o host, sem o app. O computador é acessado
normalmente (tela, teclado, arquivos, terminal), mas não tem como acessar outros, nem
interface para abrir.

**Windows** (fica como serviço, acessível sempre, mesmo sem ninguém mexer):

1. No navegador desse computador, abra `https://rdesk.seu-dominio.com/download` e baixe o
   **rdesk-host** do Windows (`rdesk-host@rdesk.seu-dominio.com.exe`).
2. Abra o **PowerShell** (menu Iniciar → digite "PowerShell"), vá para Downloads e rode
   (com a sua chave e o nome que quiser):

   ```powershell
   cd $HOME\Downloads
   .\rdesk-host@rdesk.seu-dominio.com.exe install --key rdk_COLE_A_CHAVE --name "PC da vó"
   ```

   Aceite a pergunta de administrador do Windows. Ele aparece em *Configurações →
   Aplicativos* como **rdesk host** e se atualiza sozinho.
3. Pronto. Em segundos ele aparece na sua lista em **Computadores**.

Com o **e-mail do dono** em vez da chave, troque `--key …` por `--owner voce@gmail.com`.
Sem app nesse computador, o **código** do pedido (ex.: `3F2A-91C0`) fica no registro do
host, `C:\ProgramData\rdesk\logs\host.log`. No seu app aparece o aviso **"1 computador
aguarda sua aprovação"** → **Revisar** → confira se o código é o mesmo → **Aprovar…**
(escolha o nome e quem pode ver).

> Quer o app completo nesse computador também (para acessar outros a partir dele)? Baixe o
> `rdesk@rdesk.seu-dominio.com.exe` e rode `install --only-share --key …`; aí o código do
> pedido aparece no app, aberto pelo menu Iniciar.

**Linux** (Ubuntu 22.04+, Debian 12+, Raspberry Pi OS):

1. Entre no computador com o **usuário da área de trabalho** (não como root), na sessão
   **X11**, e configure o **login automático** desse usuário.
2. No terminal (com a sua chave e o nome):

   ```sh
   curl -fsSL https://rdesk.seu-dominio.com/install.sh | sudo sh -s -- --host --key rdk_COLE_A_CHAVE --name "Servidor da loja"
   ```

   Isso instala o **`rdesk-host`** (só o host, sem o app), liga as atualizações e liga o host
   como serviço desse usuário (ele sobe sozinho em todo boot).
3. Pronto: ele aparece na sua lista.

Com o e-mail: troque `--key …` por `--owner voce@gmail.com`. O código que você vai
conferir aparece em `journalctl --user -u rdesk-host -f`.

**Servidor Linux sem tela** (uma VPS, um servidor no rack: Ubuntu 22.04+, Debian 12+, x86_64
ou ARM64):

O mesmo **`rdesk-host`**, agora como serviço de sistema. Sem ambiente gráfico não há imagem
para ver: ele percebe isso sozinho e a sessão abre o **terminal**, os **arquivos** e os
**túneis de porta**, como o usuário escolhido.

1. Entre por SSH com o usuário cujo terminal e arquivos vão ser usados (não como root).
2. Rode (com a sua chave e o nome):

   ```sh
   curl -fsSL https://rdesk.seu-dominio.com/install.sh | sudo sh -s -- --headless --key rdk_COLE_A_CHAVE --name "VPS"
   ```

   Isso instala o `rdesk-host`, liga as atualizações e liga o serviço de sistema
   `rdesk-host@SEU_USUÁRIO` (sobe sozinho em todo boot, sem ninguém logado). Para outro
   usuário, acrescente `--user NOME`.
3. Pronto: ele aparece na sua lista. Ao conectar, a janela abre direto em **Arquivos** e
   **Terminal**; pela linha de comando, `rdesk shell "VPS"` e `rdesk tunnel "VPS" -L …`.

Com o e-mail: troque `--key …` por `--owner voce@gmail.com`; o código aparece em
`journalctl -u rdesk-host@SEU_USUÁRIO -f`. O `rdesk-host` e o `rdesk` não ficam instalados
juntos: instalar um remove o outro.

**Mac** (ou qualquer computador, pelo app):

1. [Instale o app](03-instalar-o-app.md) (no Mac, dê as permissões de Gravação de Tela e
   Acessibilidade).
2. Na tela de entrada, **não** entre com conta: clique em **Só compartilhar este
   computador**.
3. Escolha **E-mail do dono** (digite o seu e-mail) ou **Chave de cadastro** (cole a
   chave), dê um nome e clique em **Pedir para entrar**.
   - Com e-mail: o app mostra um **código** (ex.: `3F2A-91C0`). No seu app, em
     **Computadores → Revisar**, confira se o código é o mesmo e clique em **Aprovar…**.
   - Com chave: entra na hora.
4. Deixe o app abrindo com o sistema: no Mac, no Terminal, rode uma vez

   ```sh
   /Applications/rdesk.app/Contents/MacOS/rdesk autostart login
   ```

   (no Windows: `rdesk autostart login`; mas no Windows prefira o jeito com serviço acima).
5. Configure o computador para **não dormir** ([como](#c-só-ser-acessado-com-a-sua-conta)).

**Depois de entrar**, o computador é seu: renomeie, compartilhe e veja o histórico como
qualquer outro (veja [Compartilhar](06-compartilhar.md)).

> Pedidos pendentes expiram em 2 dias. Um pedido **recusado** fica recusado por 30 dias.

---

## E. Vários computadores de uma vez (chave de cadastro)

*Exemplo: 10 Raspberry Pi, os PCs de uma sala, uma imagem pronta.*

1. No seu app: **Computadores → ＋ Adicionar → Vários computadores (chave de cadastro) →
   Nova chave…**, com **Uso: vários computadores**, a validade que precisar (até 365 dias)
   e **Quem pode ver os computadores que ela adiciona** (por exemplo, compartilhado com o
   grupo "Laboratório", com *controle total*). **Criar chave** e copie.
2. Em cada computador, use a mesma chave:
   - **Windows**: `.\rdesk-host@rdesk.seu-dominio.com.exe install --key rdk_…`
     (o nome é o do computador; use `--name` para outro);
   - **Linux**: `curl -fsSL https://rdesk.seu-dominio.com/install.sh | sudo sh -s -- --host --key rdk_…`.
3. Cada um aparece na lista assim que liga, já compartilhado como a chave mandou.
4. Terminou? **Revogue a chave** (em *Configurações → Chaves de cadastro*, ou na mesma tela
   do assistente → **Revogar**). Os computadores que já entraram continuam.

### Imagem pronta (gravar o mesmo cartão SD em várias placas)

1. Monte a imagem: Raspberry Pi OS (ou outro Linux), sessão X11 com login automático.
2. Instale com a chave, como no passo 2 acima.
3. **Antes de copiar a imagem, apague a identidade**: `rm -rf ~/.config/rdesk/`. Cada placa
   cria o próprio ID do host no primeiro boot. Placas com a mesma pasta teriam o mesmo ID
   e uma derrubaria a outra.
4. Deixe a chave em `~/.config/rdesk-enroll.env` (o instalador já criou esse arquivo).
5. Grave os cartões. No primeiro boot, cada placa entra sozinha.
6. Revogue a chave quando terminar.

---

## F. Sem servidor (rede local ou Tailscale)

*Exemplo: computadores da mesma casa, sem montar servidor. Ou de qualquer lugar, pelo
Tailscale.*

Sem servidor, cada computador é protegido por **uma senha ou chave SSH dele mesmo**, e só
aceita conexões da **rede local** (o mesmo Wi-Fi/roteador) ou do **Tailscale**. Qualquer
outra origem é recusada antes mesmo de pedir a senha.

### F1. No computador que vai ser acessado

1. [Instale o app](03-instalar-o-app.md). Na tela de entrada, clique em **Usar nesta rede,
   sem servidor**.
2. Vá em **Este computador → Segurança** e crie a forma de entrar:
   - **Senha**: **Definir senha…** → **Gerar uma forte** → **Copiar** e guarde no seu
     gerenciador de senhas (ex.: 1Password) → **Salvar senha**; e/ou
   - **Chave SSH**: **Adicionar chave SSH…** e escolha uma chave do agente do 1Password
     (ou cole a chave pública de quem vai acessar).

   Sem senha nem chave, o host não liga.
3. Ainda em **Segurança**, confira **Acesso direto**: ligado, de **rede local** e/ou
   **Tailscale**. (Opcional: ligue **Anunciá-lo na rede local (mDNS)** para conectar só pelo ID, sem
   digitar o endereço; quem estiver na rede vê que há um rdesk ali.)
4. Em **Visão geral**, ligue o **interruptor**.
5. Copie o **ID do host** (**Copiar**) e mande para quem vai acessar (por e-mail, chat…).
   O ID não é segredo, mas confira que chegou inteiro.
6. Descubra o **endereço** do computador:
   - Windows: PowerShell → `ipconfig` → "Endereço IPv4" (ex.: `192.168.0.20`);
   - Mac: *Ajustes → Rede* → o IP da conexão ativa;
   - Linux: `ip -4 addr` (o `inet` da rede, ex.: `192.168.0.20`);
   - Tailscale: o IP `100.x.y.z` ou o nome do computador no Tailscale.

   Dica: reserve um IP fixo para ele no roteador, para o endereço não mudar.
7. **Windows**: se o Windows perguntar se o rdesk pode usar a rede, permita em **redes
   privadas**. (Com o serviço instalado — **Instalar** na *Visão geral* — a regra do
   firewall é criada sozinha, já limitada à rede local e ao Tailscale.)

### F2. No computador que vai acessar

1. [Instale o app](03-instalar-o-app.md). Entre com **Usar nesta rede, sem servidor** (ou
   com a sua conta, se tiver servidor: funciona igual).
2. **Computadores → ＋ Adicionar → Um computador nesta rede, sem servidor**:
   - **Nome**: como quiser chamar;
   - **ID do host**: o que você recebeu;
   - **Endereço**: o IP ou o nome do Tailscale (vazio = procurar na rede pelo anúncio, se
     o outro ligou essa opção);
   - **Login**: *Senha* ou *Chave do 1Password* (ou *Chave, depois senha*);
   - **Senha salva** (opcional): digite a senha para o app guardá-la no cofre do sistema e
     não perguntar toda vez;
   - **Salvar**.
3. Ele aparece na lista com a marca **Nesta rede**. **Conectar**. Se for por senha, o app
   pede a senha na primeira vez.

Pela linha de comando: `rdesk connect <ID do host>@192.168.0.20`.

### De qualquer lugar, sem servidor: Tailscale

O [Tailscale](https://tailscale.com) cria uma rede privada entre os seus computadores,
onde quer que estejam. É o jeito de usar o rdesk pela internet **sem montar um servidor**:

1. Instale o Tailscale nos dois computadores e entre com a **mesma conta**.
2. No computador acessado, deixe **Tailscale** marcado no acesso direto (passo F1.3).
3. No outro, use como **Endereço** o IP `100.x.y.z` ou o nome do Tailscale do computador.

---

## G. Servidor e rede local juntos (acesso direto)

*Exemplo: um computador cadastrado no servidor que também deve abrir direto pela rede de
casa, mesmo se a internet ou o servidor cair.*

Num computador **cadastrado**, o acesso direto vem **desligado**: só entra quem tem
permissão no servidor. Para ligar a "porta dos fundos" da rede local:

1. No computador acessado: **Este computador → Segurança**:
   - crie uma **senha** e/ou **chave SSH** própria (como no [F1](#f1-no-computador-que-vai-ser-acessado));
   - ligue **Acesso direto** e escolha **rede local** e/ou **Tailscale**;
   - clique em **Aplicar** na barra que aparece embaixo.
2. No computador que acessa: adicione-o também como **Um computador nesta rede** (como no
   [F2](#f2-no-computador-que-vai-acessar)), com o endereço local.
3. Agora há duas linhas na lista: a do servidor (entra com a sua permissão, de qualquer
   lugar) e a **Nesta rede** (entra com a senha/chave, só da rede local).

Para quem tem acesso pelo servidor, o app avisa que o acesso direto está ligado nesse
computador, e as sessões diretas aparecem no histórico.

---

## H. Quero a ligação direta, sem passar pelo relay

Primeiro, a boa notícia: **isso já é automático**. O relay só apresenta os dois
computadores; em seguida eles tentam se ligar direto, e na maioria das redes conseguem.
O relay só repassa os dados quando a rede não deixa.

**Como saber**: na janela da sessão, abra **Info** (painel lateral). A linha **Rota** diz
**direta** ou **relay**. A latência também aparece na barra do topo.

**Se fica sempre no relay**, as causas comuns são: as duas pontas atrás de CGNAT (comum em
internet móvel e em algumas operadoras), firewalls corporativos que bloqueiam UDP, ou a
porta **7842/udp** fechada no servidor. O que ajuda:

1. Confira a **7842/udp** aberta no servidor (no `ufw` e no painel do provedor).
2. Use **IPv6** se as duas redes tiverem (o rdesk usa sozinho).
3. Para um computador fixo (o de casa), **redirecione a porta UDP 7420** no roteador para
   ele, e no computador que acessa informe o endereço: selecione o computador →
   **⋯ → Opções de login → Avançado → Endereço** (ex.: `meu-ip-ou-ddns:7420`). O app tenta
   esse endereço direto primeiro.

**Sem relay de jeito nenhum** (nem servidor): use o [cenário F](#f-sem-servidor-rede-local-ou-tailscale)
— na rede local ou pelo Tailscale, a ligação é sempre direta.

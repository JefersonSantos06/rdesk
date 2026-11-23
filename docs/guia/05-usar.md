# 5. Usar no dia a dia

## A lista de computadores

Em **Computadores** ficam todos os computadores que você pode acessar: os seus, os
compartilhados com você e os salvos neste aparelho para a rede local.

- **Buscar ou conectar** (atalho **Ctrl+K**): digite parte do nome, o e-mail do dono ou o
  começo do ID para filtrar. **Enter** conecta ao selecionado. Também aceita um ID
  completo, ou `ID@endereço` para um computador da rede local.
- **Filtros**: *Todos*, *Meus*, *Compartilhados comigo*, *Nesta rede* e *Só online*.
- Em cada linha, a situação ao vivo:

  | Situação | Significa |
  |---|---|
  | **Online** | ligado e pronto |
  | **Em uso** | alguém está conectado nele agora (se você conectar, a sessão dessa pessoa cai) |
  | **Sessão aberta** | você já tem uma janela aberta com ele |
  | **Offline · visto há 3 h** | desligado, sem internet, ou com o host desligado |

- **Clique** seleciona, **dois cliques** (ou **Enter**) conecta, as **setas** mudam a
  seleção e o **botão direito** abre as ações.
- O **painel da direita** mostra o selecionado: **Conectar**, **Arquivos**, **Terminal**
  (quando o seu acesso permite), **Ligar** num computador desligado (outro computador
  ligado na mesma rede manda o sinal de Wake-on-LAN), o ID do host, quem tem acesso, as últimas sessões e o
  menu **⋯** (copiar o ID, opções de login). Numa janela pequena, use o **>** da linha para
  ver os detalhes.
- **F2** renomeia (nos seus) e **Delete** descadastra (só o dono, com confirmação).
- O computador em que você está vem **primeiro**, com o selo **Este computador**: dele não
  se abre a tela nem os arquivos, só o terminal (veja
  [No próprio computador](06-compartilhar.md#no-próprio-computador)).

Se a janela de um computador já estiver aberta, **Mostrar** (no lugar de *Conectar*) traz
ela para a frente. **Arquivos** e **Terminal** também abrem direto nela.

## Mais de um servidor

Você pode entrar em vários servidores rdesk no mesmo app: o seu, o da empresa X, o da
empresa Y. Cada um com o e-mail que você usa nele (podem ser e-mails diferentes, e até dois
e-mails no mesmo servidor).

O app mostra **um servidor de cada vez**. A lista de computadores, os grupos e o admin são
sempre os do servidor em uso, e nada de um aparece no outro.

- **Trocar**: clique na conta no pé da barra lateral (nome do servidor e e-mail). Abre a
  lista dos servidores salvos; escolha outro. Os outros continuam logados (cada um pelos
  seus 10 dias), então trocar não pede login de novo.
- **Adicionar**: no mesmo menu, **Adicionar servidor**. Entre pelo navegador como sempre;
  o novo servidor fica salvo e passa a ser o em uso. **Voltar** desiste e fica no atual.
- **Gerenciar**: em *Configurações → Conta* ficam o servidor em uso e os **outros
  servidores**, cada um com **Usar** e **⋯ → Sair**.
- Se o login de um servidor terminou (10 dias, ou o admin encerrou), ele continua na lista
  com *entre de novo*: escolhê-lo abre o login já com o endereço dele.

As **janelas já abertas continuam** quando você troca: cada uma fica no servidor de onde
veio (o título mostra o servidor e o e-mail) e reconecta por ele.

O **host** deste computador não muda com a troca: ele continua cadastrado no servidor em
que foi cadastrado e acessível por quem tem acesso lá.

## Conectar

1. Selecione o computador e clique em **Conectar**.
2. Abre uma janela nova. Enquanto conecta, ela mostra *Conectando a …*; você pode
   **Cancelar**.
3. Se o computador tiver o **cadeado extra**, o app pede a chave (o 1Password pergunta) ou
   a senha dele.
4. Pronto: a tela do outro computador aparece. Mexa o mouse e digite normalmente.

## A janela da sessão

No topo há uma barra (em tela cheia ela se esconde; encoste o mouse no alto da tela para
ela voltar):

| Na barra | Para quê |
|---|---|
| ● nome · **ms** | quem você está acessando e a latência (passe o mouse para ver se a ligação é direta ou pelo relay) |
| **Monitor** | escolher qual tela ver, se o outro tiver várias; *Atualizar imagem* se algo ficou estranho |
| **Qualidade** | *Auto* (se adapta à conexão: baixa a qualidade e o tamanho quando ela não acompanha, sobe quando folga; mostra o que escolheu), *Velocidade* (mais fluido, menos nítido), *Qualidade* (mais nítido); ou ajuste o fps e a qualidade à mão. Muda na hora. |
| **Teclas** | enviar **Ctrl+Alt+Del** e a **tecla Windows** (em computadores Windows), **Alt+Tab**, **Caps Lock**, **Num Lock**, e ligar as **teclas de Mac** |
| **Sessão** | ouvir o som do computador, **tela preta lá**, **bloquear o teclado e o mouse de lá**, **túnel de porta** e **reiniciar** (normal ou em modo de segurança). Aparece o que o outro computador oferece. |
| **Arquivos** | abre o painel de arquivos |
| **Terminal** | abre o painel de terminal |
| ⛶ | tela cheia |
| **Desconectar** | encerra a sessão |

Quando a janela é estreita, os botões viram só ícones (passe o mouse para ver o nome).

### Teclas especiais

- **Ctrl+Alt+Del** pela barra (*Teclas*). No Windows, só funciona com o **serviço do
  Windows** instalado no outro computador.
- **Do Mac para Windows/Linux**: as **teclas de Mac** vêm ligadas: **Cmd** vira **Ctrl**
  (então Cmd+C copia), o **Option esquerdo** vira a tecla **Windows**, e o **Control** vira
  **Alt**. O Option direito continua AltGr. Desligue em *Teclas* se preferir.
- **Caps Lock** e **Num Lock** acompanham o seu teclado: ao conectar, ao voltar para a
  janela e sempre que você os liga ou desliga, o outro computador fica igual (Windows e Linux
  com X11). O Mac não tem Num Lock (o teclado numérico dele sempre digita números): o do
  outro computador fica como está, e a tecla Clear de um teclado numérico de Mac faz as vezes
  dele. Em computadores com uma versão anterior do rdesk, ou para mudá-los só lá, use
  *Teclas* → *Caps Lock* ou *Num Lock*.
- Atalhos do seu próprio sistema (como Alt+Tab no Windows) ficam com o seu computador; use o
  menu *Teclas* para mandá-los ao outro.

### Copiar e colar

Texto e imagens (prints, por exemplo) copiados de um lado podem ser colados no outro, nos
dois sentidos, automaticamente (só com **controle total**; imagens entre computadores com rdesk
0.7 ou mais novo). Para desligar no computador acessado:
*Este computador → Desempenho e arquivos → Área de transferência*.

### Som, cursor e o menu Sessão

- **Som**: o que toca no computador acessado (Windows) toca aqui. Desligue em *Sessão →
  Ouvir o computador*.
- **Cursor**: sobre a imagem, o seu mouse fica com o formato do cursor de lá (I sobre texto,
  mãozinha em links, setas nas bordas), sem atraso (hosts Windows).
- **Tela preta lá**: quem estiver no computador vê uma tela preta; você continua vendo a tela
  normal. Junto com **bloquear o teclado e o mouse de lá**, ninguém mexe enquanto você
  trabalha. Tudo volta ao normal quando a sessão termina (e Ctrl+Alt+Del lá devolve o teclado
  e o mouse). Windows com o serviço instalado.
- **Túnel de porta**: `13389:localhost:3389` faz a porta 13389 deste computador levar à 3389
  do outro (a área de trabalho remota dele); `8080:192.168.0.1:80` leva à página do roteador
  da rede de lá. Só com controle total.
- **Reiniciar o computador** e **Reiniciar em modo de segurança** (com rede, para remover o que
  impede o Windows de iniciar): a janela reconecta sozinha quando ele voltar; depois do modo
  de segurança, o próximo reinício é normal.

Se outra pessoa já estiver conectada, a sua conexão é recusada com "em uso por …" (a sessão
dela não cai). Se o dono ligou *Pedir permissão antes de cada sessão*, quem estiver lá precisa
permitir.

### Arquivos

1. Clique em **Arquivos** na barra. Abre um painel ao lado da tela.
2. **Para baixar**: navegue pelas pastas (clique nos nomes, ou na seta para cima; clique no
   caminho para digitar um) e clique no ícone de download ao lado do arquivo (ou dois
   cliques nele). Vai para a sua pasta **Downloads** (o painel mostra onde, com o botão
   **Abrir**).
3. **Para enviar**: **Enviar arquivos…**, ou **arraste os arquivos para a janela**. Eles vão
   para `Downloads/rdesk` do outro computador.
4. Embaixo do painel ficam as transferências: andamento, concluídas e as que falharam.
   **Limpar concluídos** limpa a lista.

Se a conexão cair no meio, a transferência continua de onde parou quando você mandar de
novo. Arquivos com o mesmo nome não são sobrescritos: viram `nome (1).ext`.

Com o serviço do Windows, os arquivos vão para os Downloads de quem está logado no Windows
naquele momento. Com ninguém logado, o envio é recusado.

### Terminal

1. Clique em **Terminal** na barra. Abre um terminal do outro computador, como quem está
   logado lá (no Windows, PowerShell; no Mac e no Linux, o shell da pessoa).
2. **＋** abre outro terminal em outra aba. Em computadores **Windows** com **controle
   total**, o **＋** oferece também **Terminal (admin)**: com os direitos de administrador
   da pessoa logada (precisa do serviço instalado lá).
3. **Copiar**: selecione o texto com o mouse e use **Ctrl+Shift+C** (ou **Ctrl+C** com algo
   selecionado; sem seleção, Ctrl+C interrompe o comando, como sempre). **Colar**:
   **Ctrl+V** ou **Ctrl+Shift+V**. O botão direito também tem *Copiar* e *Colar*.
4. Role com a roda do mouse para ver o histórico; digite qualquer coisa para voltar ao fim.
5. Fechar a aba de um terminal ativo pede confirmação (o que estiver rodando nele para).

Quando o terminal está aberto ao lado da tela, um selo na barra diz para onde vai o
teclado: **Teclado: terminal** ou **Teclado: tela remota**. Clique nele (ou na imagem)
para trocar.

### Info

O painel **Info** mostra o computador, o sistema, o seu nível de acesso, a **rota**
(direta ou relay), a latência, os quadros por segundo e a velocidade.

## Quando a conexão cai

- Quedas curtas: a imagem só pausa e volta sozinha.
- Quedas maiores: a imagem escurece e aparece **Reconectando a …**, com há quanto tempo
  caiu, o motivo e a tentativa. **O teclado e o mouse ficam pausados** até voltar (nada do
  que você digitar agora é enviado depois).
  - **Tentar agora** pula a espera; **Desconectar** desiste.
  - Depois de 20 segundos, o app avisa que o computador pode estar desligado ou sem
    internet. Ele continua tentando até você desistir.
- Quando volta: aparece **Reconectado após …** por alguns segundos.
- Se a sessão terminou de vez (alguém conectou no seu lugar, ou foi encerrada pelo outro
  lado), aparecem **Reconectar** e **Fechar**.

## Fechar

- **Desconectar** (ou fechar a janela) encerra a sessão. Se houver um terminal ativo ou
  transferências em andamento, o app pergunta antes.
- No computador acessado, quem está na frente da tela sempre vê **quem está conectado**:
  no Windows, uma barra no alto da tela com o botão **Encerrar**, que derruba a sessão na
  hora.

## Do outro lado: "Este computador"

Em **Este computador** você vê e controla o que acontece **neste** computador:

- o **interruptor** (ninguém consegue se conectar com ele desligado), o **ID do host** com
  **Copiar**, e por onde chegam até ele;
- **quem está conectado agora**, com **Desconectar**;
- **Configuração do acesso**: cadastro no servidor, serviço do Windows, senha própria,
  chaves SSH;
- abas **Segurança**, **Desempenho e arquivos** (porta, pasta dos arquivos recebidos,
  fps, qualidade, área de transferência, terminal como administrador), **Atividade**
  (quem conectou, logins que falharam) e **Diagnóstico** (**Verificar** testa permissões,
  monitores e rede).

Mudanças que reiniciam o host (como a porta) só valem depois de **Aplicar**, na barra que
aparece embaixo.

## Bandeja (ícone perto do relógio)

*Abrir o rdesk*, a situação do host, *Ligar/Desligar o host*, *Copiar o ID do host* e
*Sair do rdesk*.

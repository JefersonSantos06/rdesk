# O app do celular

Com o app do celular você acessa os seus computadores pelo **Android** ou pelo **iPhone**: vê a tela, usa o mouse e o teclado, troca arquivos e abre um terminal. O celular só acessa: ninguém acessa o celular.

## Instalar

### Android

1. No celular, abra `https://seu-servidor/download` e toque em **rdesk.apk**.
2. Abra o arquivo baixado. O Android pede para **permitir instalar apps desta fonte**: permita (só para o navegador que baixou).
3. Instale e abra o **rdesk**.

As próximas versões aparecem no próprio app, num aviso no topo da lista: **Atualizar**. O Android confirma antes de instalar.

### iPhone

O app não está na App Store. Ele é instalado a partir de um computador, assinado com o seu Apple ID:

1. Baixe **rdesk.ipa** em `https://seu-servidor/download` (no computador).
2. Instale o [Sideloadly](https://sideloadly.io) (Windows ou Mac) e conecte o iPhone no cabo.
3. Arraste o `rdesk.ipa` para o Sideloadly, coloque o seu Apple ID e clique em **Start**.
4. No iPhone: *Ajustes → Geral → VPN e Gerenciamento de Dispositivo*, toque no seu Apple ID e em **Confiar**.

Com um Apple ID gratuito, a instalação vale **7 dias**: reinstale do mesmo jeito (os seus dados ficam). Com o [AltStore](https://altstore.io), a renovação é automática pelo Wi-Fi. Com uma conta de desenvolvedor da Apple (US$ 99 por ano), vale um ano.

## Entrar

- **Com servidor**: digite o endereço do servidor e toque em **Entrar pelo navegador**. É o mesmo login do computador (Google ou GitHub e o segundo fator), e no fim o navegador volta sozinho para o app.
- **Sem servidor** (rede de casa ou Tailscale): **Usar nesta rede, sem servidor**. Depois, em **＋ Nesta rede**, coloque o nome, o **ID do host** (o computador mostra em *Este computador*) e o endereço (IP ou nome do Tailscale). No Android, o endereço pode ficar vazio se o computador anuncia na rede (mDNS). No iPhone, coloque sempre o endereço.

### Mais de um servidor

Toque no título da página de computadores (o servidor e o e-mail em uso) para **trocar de
servidor** ou **adicionar servidor**. Cada servidor guarda o seu login e o seu e-mail; a
lista mostra só os computadores do servidor em uso. Uma sessão aberta continua no servidor
de onde veio. Também em *Configurações → Conta → Trocar servidor*.

## Usar

Toque num computador para conectar. Tocar e segurar mostra as opções: **Arquivos**, **Terminal** e copiar o ID.

Na primeira vez, o app mostra os gestos. Eles também ficam no botão flutuante (**Gestos**) e em *Configurações → Controle remoto*.

### Modo touchpad (o padrão)

A tela funciona como o touchpad de um notebook:

| Gesto | O que faz |
|---|---|
| Deslizar um dedo | move o ponteiro: devagar é preciso, rápido vai longe |
| Tocar | clique |
| Dois toques | clique duplo |
| Tocar, tocar de novo e deslizar | arrastar (selecionar texto, mover janelas) |
| Tocar e segurar | botão direito |
| Deslizar dois dedos | rolar (um movimento rápido continua rolando) |

### Modo toque direto

Você toca na tela do computador onde quer, como numa tela sensível ao toque:

| Gesto | O que faz |
|---|---|
| Tocar | clique ali mesmo |
| Deslizar um dedo | rolar |
| Tocar e segurar | uma lupa acima do dedo mostra o ponto exato |
| …e deslizar | arrastar |
| …ou soltar | botão direito |
| Deslizar dois dedos | mover a vista ampliada |

### Nos dois modos

| Gesto | O que faz |
|---|---|
| Pinça | aumentar e diminuir |
| Tocar com dois dedos | botão direito |
| Dois toques com dois dedos | ampliar ali, ou voltar |
| Tocar com três dedos | abrir ou fechar o teclado |

Um mouse ligado no celular (Bluetooth ou USB-C) funciona como no computador.

### O botão flutuante

O botão redondo no canto abre as ferramentas. Arraste-o para onde atrapalhar menos. A bolinha mostra a conexão: verde, boa; amarela, lenta; vermelha, caída.

- **Teclado** e **Teclas**: a barra de teclas aparece sozinha sobre o teclado do celular.
  - Ctrl, Alt, Shift e Win ficam presos até a próxima tecla, então Ctrl + C no teclado do celular vira Ctrl+C no computador.
  - Também tem Esc, Tab, setas, **Copiar**, **Colar**, **Recortar**, **Desfazer** e **Tudo** (com ⌘ num Mac), Home/End, PgUp/PgDn, Del e F1–F12.
- **Touchpad** ou **Toque direto**: troca o modo. O app lembra a escolha.
- **Gestos**: as tabelas acima.
- **Tela** (com vários monitores).
- **Qualidade**: *Velocidade* para dados móveis; *Resolução: completa* para ver a imagem sem redução.
- **Girar**: automático, deitado ou em pé. Deitado, a tela do computador fica maior.
- **Atalhos**: Ctrl+Alt+Del, Ctrl+Shift+Esc, Alt+Tab e a tecla Windows (num Mac, ⌘+Tab e ⌘+Espaço).
- **Enviar copiado**: manda o texto copiado no celular para o computador.
- **Arquivos** e **Terminal**.
- **Desconectar**.

Em *Configurações → Controle remoto* ficam o modo de toque padrão e a velocidade do ponteiro.

O que o computador copia vai para a área de transferência do celular.

## Computadores com cadeado extra

Se o computador pede a própria senha ou chave além do servidor, o app oferece duas formas:

- **Senha**: o app pergunta e pode **lembrar neste celular**.
- **Chave deste celular**:
  1. No app, em *Configurações → Chave deste celular*, toque em **Criar a chave deste celular**.
  2. Copie a linha que aparece (começa com `ecdsa-sha2-nistp256`).
  3. No computador, cole a linha em *Este computador → Segurança → Chaves SSH*.

  A partir daí, o celular entra com a digital ou o rosto. A chave fica no chip de segurança do celular e não sai dele, nem num backup.

## Acessar este celular (Android, em teste)

O celular Android também pode ser o acessado: quem se conectar a ele vê a tela dele e, com o **controle por toque** ligado, também toca, rola e digita nele.

1. No app, em *Configurações → Este celular*, ligue **Permitir acesso a este celular**.
2. O Android pergunta se o rdesk pode mostrar notificações (o aviso de que a tela está sendo compartilhada, e os pedidos de acesso) e gravar áudio (para mandar o som do celular). As duas são opcionais. Depois pergunta se o rdesk pode **gravar ou transmitir a tela inteira**: toque em **Iniciar**.
3. Para entrar no celular, use uma destas formas:
   - **Pelo servidor**: toque em **Adicionar ao meu servidor**. O celular aparece na sua lista de computadores, como qualquer outro.
   - **Na rede local ou no Tailscale**: toque em **Definir uma senha** e conecte com o **ID do host** que a página mostra e o endereço do celular.

Enquanto o acesso estiver ligado, a notificação *Compartilhando a tela deste celular* fica na barra. O botão **Parar** dela desliga tudo.

O compartilhamento também acaba sozinho:

- quando a tela bloqueia, porque o Android encerra a gravação;
- quando você para a transmissão pelo próprio sistema.

Nesses casos, volte em *Este celular* e ligue de novo, ou deixe **Ligar sozinho** ligado (abaixo). O Android pede a permissão a cada vez.

### Quem entra

Com **Perguntar antes de cada conexão** ligado (o padrão no celular), cada conexão nova mostra uma notificação *rdesk: permitir acesso?* com **Permitir** e **Recusar**. Sem resposta em 30 segundos, ou com a notificação descartada, a conexão é recusada. Uma reconexão da mesma sessão não pergunta de novo.

### Ligar sozinho

Com **Ligar sozinho**, o rdesk compartilha a tela e espera conexões quando o celular liga e quando é desbloqueado. Bloquear a tela encerra o compartilhamento.

- **Com o controle por toque ligado**, o pedido do Android abre sozinho. Sem ele, o Android não deixa um app abrir telas em segundo plano, e uma notificação oferece o pedido com um toque.
- O Android pergunta **"Gravar ou transmitir?" a cada vez**. Para não perguntar mais, libere o rdesk **uma vez** pelo USB, num computador com o [adb](https://developer.android.com/tools/adb):

  ```
  adb shell appops set dev.rdesk.rdesk PROJECT_MEDIA allow
  ```

  Isso funciona até o Android 13. No 14 e mais novos, o Android pode perguntar mesmo assim (não testado). Para desfazer: `adb shell appops set dev.rdesk.rdesk PROJECT_MEDIA default`.
- O rdesk **não** destrava o celular: com bloqueio por PIN, senha ou digital, alguém precisa desbloqueá-lo.

### Controle por toque

O Android só deixa outro app tocar na tela por um **serviço de acessibilidade**, que você liga uma vez:

1. Em *Este celular → Controle por toque*, toque em **Ligar** e depois em **Abrir ajustes**.
2. Nos ajustes de acessibilidade do Android, toque em **rdesk: controle por toque**, ligue a chave e confirme.
3. Se o Android disser que a configuração é **restrita** (acontece com apps instalados fora de uma loja, no Android 13 ou mais novo):
   1. volte ao rdesk e toque em **Configuração restrita? Informações do app**;
   2. toque em **⋮** e depois em **Permitir configurações restritas**;
   3. repita os passos 1 e 2.

O serviço só age enquanto o acesso estiver ligado e alguém estiver conectado. Ele não lê a tela, só o campo em que se digita. Forçar a parada do rdesk (em *Informações do app*) desliga o serviço, e aí é preciso ligar de novo.

Do computador:

| Do lado de lá | No celular |
|---|---|
| Botão esquerdo | Toca. Segurado, é o dedo encostado: arrasta, e parado vira toque longo |
| Botão direito, Esc, botão Voltar do mouse | Voltar |
| Botão do meio, tecla Windows | Início |
| Tecla Menu, botão Avançar do mouse | Apps recentes |
| Roda do mouse | Rola a lista sob o ponteiro |
| Teclado | Digita no campo em foco; Backspace, Delete, Enter, setas, Home e End editam; Ctrl+A/C/X/V selecionam, copiam, recortam e colam |
| Print Screen | Captura de tela; as teclas de volume mudam o volume |

No app do computador, a barra de cima ganha **Voltar**, **Início** e **Recentes** quando o outro lado é um celular, e o menu da sessão tem **Notificações**, **Ajustes rápidos**, **Captura de tela** e **Bloquear o celular**. O que você digita vai como texto, então acentos, Ç e o teclado ABNT2 saem certos (com o app do computador desta versão ou mais novo; com um mais antigo, as letras saem como num teclado americano).

### De um celular para outro

No app do celular que acessa, quando o outro lado é um celular, a folha de ferramentas tem **Voltar**, **Início**, **Recentes** e **Notificações**, e a opção **Toques vão para o celular** (ligada): os seus dedos tocam o outro celular como estão, com pinça e gestos de dois dedos. Desligada, os gestos voltam a ser os de sempre, e a pinça amplia a imagem aqui.

### Arquivos, área de transferência e som

- **Arquivos**: o que o outro lado envia vai para *Download/rdesk*. Para ele ver e baixar o resto dos arquivos do celular, toque em **Permitir** em *Este celular → Arquivos* (o Android chama de *Acesso a todos os arquivos*).
- **Área de transferência**: o que o outro lado copia chega à área de transferência do celular. O contrário não existe, porque o Android não deixa nenhum app ler o que foi copiado em segundo plano. Para mandar um texto do celular, abra o app do rdesk no celular e use *Enviar copiado* numa sessão.
- **Som**: com a permissão de áudio, quem se conectar ouve o que o celular toca (Android 10 ou mais novo). Alguns apps não deixam o som deles ser gravado e ficam mudos.

### Bom saber

- **Movimento**: quando a tela inteira se mexe (rolar, abrir apps), o celular manda vídeo H.264 feito pelo chip de vídeo dele. Parada, a tela volta a ir em blocos nítidos.
- **Resolução**: a tela vai com no máximo 1920 pixels no lado maior (o limite dos codificadores de vídeo dos celulares).
- **Telas protegidas**: apps de banco e campos de senha aparecem pretos (o Android esconde do compartilhamento). No Android 15, as notificações com códigos também.
- **Wake-on-LAN**: o celular não pode ser ligado pela rede, mas pode ligar outros computadores da rede dele quando o servidor pede.
- **Terminal**: abre o shell do Android, com as permissões do app (poucas).

## Bom saber

- **Ao sair do app**: o celular desliga a rede dele, e a sessão cai. Ao voltar, ela reconecta sozinha, sem pedir senha de novo (por até 10 minutos).
- **Dados móveis**: a imagem já vem do tamanho da tela do celular. Para gastar menos, use *Qualidade → Velocidade*.
- **Versões**: o celular precisa de um computador com o rdesk desta versão ou mais novo para reduzir a imagem e mostrar o ponteiro. Com versões antigas, conecta do mesmo jeito, com a imagem inteira.

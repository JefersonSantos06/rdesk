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

## Usar

Toque num computador para conectar. Tocar e segurar mostra as opções: **Arquivos**, **Terminal** e copiar o ID.

Na tela do computador, no **modo touchpad** (o padrão):

| Gesto | O que faz |
|---|---|
| Arrastar um dedo | move o ponteiro |
| Tocar | clique |
| Tocar e segurar, e arrastar | arrastar (segura o botão) |
| Tocar com dois dedos | botão direito |
| Arrastar com dois dedos | rolar |
| Pinça | zoom (o ponteiro puxa a tela) |
| Tocar com três dedos | abrir ou fechar o teclado |

No **modo toque direto** (botão da mão na barra), você toca onde quer clicar. Tocar e segurar é o botão direito.

A barra de cima (o ✕ desconecta):

- **teclado**;
- **teclas**: Ctrl, Alt, Shift e Win ficam presos até a próxima tecla, então Ctrl + C no teclado do celular vira Ctrl+C no computador; também Esc, Tab, setas e F1–F12;
- **modo de toque**;
- **tela** (com vários monitores);
- **qualidade**: *Velocidade* para dados móveis; *Resolução: completa* para ver a imagem sem redução;
- **enviar a área de transferência**;
- **arquivos**;
- **terminal**;
- **atalhos** (Ctrl+Alt+Del, Alt+Tab).

Para esconder a barra, use a seta à direita; uma alça no topo traz a barra de volta.

O que o computador copia vai para a área de transferência do celular. Para mandar o texto do celular, use o botão da barra.

## Computadores com cadeado extra

Se o computador pede a própria senha ou chave além do servidor, o app oferece duas formas:

- **Senha**: o app pergunta e pode **lembrar neste celular**.
- **Chave deste celular**:
  1. No app, em *Configurações → Chave deste celular*, toque em **Criar a chave deste celular**.
  2. Copie a linha que aparece (começa com `ecdsa-sha2-nistp256`).
  3. No computador, cole a linha em *Este computador → Segurança → Chaves SSH*.

  A partir daí, o celular entra com a digital ou o rosto. A chave fica no chip de segurança do celular e não sai dele, nem num backup.

## Bom saber

- **Ao sair do app**: o celular desliga a rede dele, e a sessão cai. Ao voltar, ela reconecta sozinha, sem pedir senha de novo (por até 10 minutos).
- **Dados móveis**: a imagem já vem do tamanho da tela do celular. Para gastar menos, use *Qualidade → Velocidade*.
- **Versões**: o celular precisa de um computador com o rdesk desta versão ou mais novo para reduzir a imagem e mostrar o ponteiro. Com versões antigas, conecta do mesmo jeito, com a imagem inteira.

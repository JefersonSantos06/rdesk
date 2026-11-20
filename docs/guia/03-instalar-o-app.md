# 3. Instalar o app

O mesmo app serve para **acessar** e para **ser acessado**. Instale em cada computador.

O jeito mais fácil de baixar é pela página do seu servidor: **`https://rdesk.seu-dominio.com/download`**.
Ela sempre mostra a versão que o admin liberou. As versões também ficam nos
[Releases do GitHub](https://github.com/JefersonSantos06/rdesk/releases).

- [Windows](#windows)
- [Mac](#mac)
- [Linux](#linux)
- [Primeiro uso](#primeiro-uso-em-qualquer-sistema)

## Windows

1. Na página de download, clique no link do **Windows**. Vem um arquivo chamado
   `rdesk@rdesk.seu-dominio.com.exe`: o nome já diz qual é o seu servidor, então não
   precisa digitar o endereço depois.
2. Dê dois cliques. Se o Windows mostrar **"O Windows protegeu o computador"**
   (SmartScreen), clique em **Mais informações** → **Executar assim mesmo**. O aviso
   aparece porque o programa não tem assinatura comercial.
3. O app abre (ele já funciona sem instalar). Faça o [primeiro uso](#primeiro-uso-em-qualquer-sistema).
4. **Instale** (recomendado): em **Este computador**, na lista *Configuração do acesso*,
   clique em **Instalar**. O Windows pede permissão de administrador uma vez. Isso:
   - copia o programa para `C:\Program Files\rdesk` e cria o atalho **rdesk** no Menu
     Iniciar;
   - instala o **serviço do Windows**, que mantém este computador acessível antes de alguém
     entrar no Windows, na tela de bloqueio e nas janelas de administrador (UAC);
   - abre o app sozinho quando você entra no Windows (na bandeja, perto do relógio);
   - aparece em **Configurações → Aplicativos**, para desinstalar como qualquer programa;
   - instala as **atualizações** que o admin liberar, sozinho.

   Depois de instalar, use o atalho do Menu Iniciar e pode apagar o `.exe` baixado.

Prefere pela linha de comando? No PowerShell, na pasta do download:

```powershell
.\rdesk@rdesk.seu-dominio.com.exe install
```

> **Antivírus** (Kaspersky e outros): um programa que captura a tela, controla o mouse e
> escuta a rede se parece com um vírus de acesso remoto, e alguns antivírus bloqueiam. Se
> acontecer, crie uma exceção para `C:\Program Files\rdesk\rdesk.exe` (e para o `.exe`
> baixado).

## Mac

Só **Mac com Apple Silicon** (M1 ou mais novo). Em Mac Intel é preciso compilar a partir do
código.

1. Na página de download, clique em **rdesk.app (Apple Silicon)**. Abra o arquivo baixado
   para descompactar.
2. Arraste o **rdesk.app** para a pasta **Aplicativos**. (Ele precisa ficar numa pasta em
   que você pode gravar para conseguir se atualizar.)
3. Na primeira vez: **botão direito** no rdesk.app → **Abrir** → **Abrir**. O Mac avisa
   porque o app não é registrado na Apple; depois disso ele abre normalmente.
4. Faça o [primeiro uso](#primeiro-uso-em-qualquer-sistema).
5. **Se este Mac vai ser acessado**, ele precisa de duas permissões. Quando você liga o
   host (em *Este computador*), o Mac pergunta por elas; e, enquanto faltar alguma,
   *Este computador* mostra um aviso com os botões **Abrir Gravação de Tela** e **Abrir
   Acessibilidade**, que levam direto à tela certa. Em cada uma:
   1. clique em **+** (ou procure o **rdesk** na lista), escolha o **rdesk** em Aplicativos
      e **ligue a chave**;
   2. feche o rdesk (ícone na barra de menus → *Sair do rdesk*) e abra de novo.

   Pelo caminho manual: **Ajustes do Sistema** → **Privacidade e Segurança** →
   **Gravação de Tela** (ou *Gravação de Tela e Áudio do Sistema*) e **Acessibilidade**.

   Sem **Gravação de Tela**, quem conecta vê a tela preta ou nenhum monitor. Sem
   **Acessibilidade**, vê a tela mas os cliques e o teclado não funcionam.

## Linux

Funciona no **Ubuntu 24.04 ou mais novo**, **Debian 13** e **Raspberry Pi OS** atual
(trixie), em PCs e em placas ARM64.

1. Abra um terminal e rode (troque o domínio):

   ```sh
   curl -fsSL https://rdesk.seu-dominio.com/install.sh | sudo sh
   ```

   Isso instala o pacote, guarda qual é o seu servidor e liga as **atualizações
   automáticas** (verifica a cada hora).
2. Abra o **rdesk** pelo menu de aplicativos (ou digite `rdesk` no terminal). Para ele
   abrir sozinho quando você entra, ligue *Configurações → Geral → Iniciar com o sistema*.
   (Até a versão 0.5.0 o pacote não criava o ícone no menu: use o terminal.)
3. Faça o [primeiro uso](#primeiro-uso-em-qualquer-sistema).
4. **Se este computador vai ser acessado**, use a sessão gráfica **X11**: na tela de login
   do Linux, antes de digitar a senha, clique na engrenagem e escolha a opção com "Xorg"
   (ex.: *Ubuntu on Xorg*). No **Wayland** também funciona, pelo portal do sistema: na
   primeira conexão aparece uma janela pedindo para compartilhar a tela, e alguém precisa
   clicar em permitir no próprio computador.

Outras distribuições: baixe o `rdesk-<versão>-linux-x86_64.tar.gz` (ou `aarch64`) nos
Releases, descompacte e copie o `rdesk` para `/usr/local/bin/`. Para atualizar, repita.

Para um Linux que **só vai ser acessado** (sem ninguém usando), há uma instalação própria:
veja o [cenário D](04-cenarios.md#d-só-ser-acessado-sem-conta-nele).

## Primeiro uso (em qualquer sistema)

1. Abra o rdesk. Aparece a tela de entrada.
2. Em **Servidor**, digite `rdesk.seu-dominio.com` (no Windows ele já vem preenchido).
3. Clique em **Entrar pelo navegador**. O navegador abre:
   1. escolha Google ou GitHub e entre com a sua conta;
   2. digite o segundo fator (na primeira vez, você cria um: veja o
      [passo 5 do servidor](02-servidor.md#passo-5-o-primeiro-login-e-o-segundo-fator));
   3. aparece "You are signed in": volte para o app.
4. Pronto: a janela mostra a barra lateral com **Computadores**, **Este computador**,
   **Grupos**, **Configurações** (e **Admin**, se você for admin).

Se aparecer "não está liberado neste servidor", peça ao admin para liberar o seu e-mail.

Coisas úteis de saber:

- **Fechar a janela não fecha o app**: ele fica rodando na bandeja (perto do relógio no
  Windows, na barra de menus no Mac). Para sair de verdade: ícone da bandeja → **Sair do
  rdesk**. Para mudar isso: *Configurações → Geral*.
- **Idioma**: *Configurações → Geral → Language · Idioma* (português do Brasil ou inglês;
  por padrão, o do sistema).
- O login vale **10 dias**; depois o app pede para entrar de novo.
- Sem servidor? Na tela de entrada, use **Usar nesta rede, sem servidor**
  ([cenário F](04-cenarios.md#f-sem-servidor-rede-local-ou-tailscale)).

Próximo: escolha o seu caso em [Cenários de uso](04-cenarios.md).

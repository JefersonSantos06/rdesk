# Checklist de testes manuais

Rodar para cada par cliente → host relevante: **Mac → Windows**, **Linux → Windows** (e Mac → Mac/Linux quando houver).

## Rede
- [ ] Sem relay, na LAN: `rdesk connect <ID>@<ip-da-lan>` conecta; a barra mostra RTT `(direct)`.
- [ ] Logado no servidor (`rdesk login …` nas duas pontas): conecta só com o ID, de outra rede (4G); a barra começa em `(relay)` e passa a `(direct)` quando o hole punching funciona.
- [ ] Relay fora do ar: o host mostra *Relay unreachable* e conexões diretas continuam funcionando.
- [ ] ID errado (um caractere trocado) → não conecta ("no answer"), nada é enviado.

## Servidor e contas
- [ ] App sem login: só a tela de login aparece; o tray não liga o host.
- [ ] Login com Google e com GitHub; e-mail fora da lista → "not allowed".
- [ ] Primeiro login pede para cadastrar o 2FA (passkey ou TOTP); os seguintes pedem o 2FA sempre.
- [ ] Código TOTP já usado é recusado; 5 códigos errados encerram a tentativa.
- [ ] *Admin*: adicionar e-mail, tornar admin, desativar (a pessoa é deslogada), remover.
- [ ] Sessão revogada/desativada no servidor: o app volta para a tela de login ao abrir.
- [ ] Atualizando de 0.7 (com `session.json`): o app abre já logado, e o arquivo vira `accounts.json`.
- [ ] Dois servidores com e-mails diferentes: *Adicionar servidor* na barra lateral; trocar entre eles muda a lista inteira (nada de um aparece no outro), sem pedir login.
- [ ] Com uma janela aberta no servidor A, trocar para o B: a janela continua (título com A · e-mail), e depois de derrubar a rede ela reconecta pelo A.
- [ ] Login de um servidor expirado/revogado: ele fica na lista com *entre de novo*; escolher abre o login com o endereço dele.
- [ ] `rdesk server list` / `rdesk server use 2` / `rdesk machines list --account …`; o app aberto acompanha a troca feita na linha de comando em alguns segundos.
- [ ] Celular: tocar no título dos computadores troca de servidor e adiciona servidor; uma sessão aberta continua no servidor dela.

## Computadores, grupos e acesso
- [ ] *This computer* → *Register this computer*: aparece em *My computers* de quem cadastrou; continua *Online* depois de fazer logout no app.
- [ ] Privado: outra pessoa não vê o computador. Compartilhado com ela (ou com um grupo dela): aparece em *Shared with me* e conecta.
- [ ] Descompartilhar (ou tirar do grupo, ou desativar o usuário) com a sessão aberta: a sessão cai em segundos e não reconecta.
- [ ] Cadeado extra ligado: depois do tíquete, pede a senha/chave do computador.
- [ ] *View only*: a imagem aparece, mas teclado, mouse e clipboard não chegam ao host; arquivos recusados.
- [ ] *Files only*: abre direto no painel de arquivos, sem imagem; enviar e baixar funcionam.
- [ ] Reduzir de *full control* para *view only* com a sessão aberta: a sessão cai; ao reconectar, só vê.
- [ ] Co-gestor: compartilha e vê o histórico; não vê *Managers…* nem *Unregister…*.
- [ ] *History…* mostra as sessões, com duração, depois que terminam.
- [ ] `rdesk-server rotate-key` + reinício: sessões novas funcionam sem recadastrar nada.
- [ ] Admin do sistema vê todos os computadores na aba *Admin*, mas não conecta nos não compartilhados.
- [ ] *Unregister*: some da lista; o host passa a aceitar só pela LAN (senha/chave).

## Computador só acessado (sem login nele)
- [ ] Tela de login → *Only share this computer* com o e-mail do dono: mostra "Waiting for … (code XXXX-XXXX)"; fechar a janela (tray) não cancela.
- [ ] O dono vê o pedido em *Waiting for your approval* com o mesmo código; *Approve…* → o computador fica *Online* em segundos, sem reiniciar nada.
- [ ] *Refuse*: o host mostra que foi recusado e para de insistir.
- [ ] E-mail inexistente: o host fica esperando e ninguém vê pedido nenhum.
- [ ] Chave de cadastro reutilizável compartilhada com um grupo: duas máquinas entram sozinhas e o grupo as vê; *Revoke* impede a terceira.
- [ ] Raspberry Pi com a imagem e `deploy/rdesk-host.service`: primeiro boot entra sozinho; duas placas da mesma imagem têm IDs diferentes.
- [ ] Descadastrar uma máquina que entrou por chave: ao reiniciar ela vira pedido de aprovação, não volta sozinha.

## Sem servidor (acesso direto)
- [ ] Tela de login → *Use on this network, without a server*: abre só a aba Computers com *On this network*; Groups e Admin somem.
- [ ] *＋ Add…* com IP da LAN: conecta com a senha ou a chave do computador; *Files* abre os arquivos.
- [ ] Pelo nome do Tailscale (`pc.tailnet.ts.net`) de outra rede: conecta.
- [ ] Host com `mdns` ligado e endereço vazio no cliente: conecta só pelo host ID.
- [ ] Host cadastrado com acesso direto ligado: o app de quem vê o computador mostra *⚠ direct access on*; a sessão direta aparece no *History…*.
- [ ] De um IP público fora da sua rede (4G sem Tailscale): o host recusa antes do login (aparece em *Activity*/log).
- [ ] Entrar no servidor depois do modo local: as listas do servidor aparecem, e *On this network* continua embaixo.

## Serviço do Windows
- [ ] *This computer* → *Install the rdesk service*: pede permissão de administrador uma vez; o host ID continua o mesmo e a página passa a mostrar o status do serviço.
- [ ] Reiniciar o Windows e conectar **antes** de entrar: aparece a tela de login; dá para digitar a senha e entrar.
- [ ] Win+L (bloqueio) com a sessão aberta: a imagem mostra a tela de bloqueio e dá para desbloquear.
- [ ] Um prompt do UAC aparece (abrir algo como administrador no host): a imagem mostra o prompt e o clique em *Sim* funciona.
- [ ] Durante a sessão, a barra "… is connected to this computer" aparece no topo; *End session* derruba a sessão.
- [ ] Enviar arquivo: chega em `Downloads\rdesk` de quem está logado, e o arquivo é dessa pessoa; navegar pastas mostra o que ela vê.
- [ ] Sair da conta (sign out) e entrar com outra: o host segue para a nova sessão em segundos.
- [ ] Sem administrador, mudar uma opção pede *Manage as administrator*; como administrador, salvar reinicia o host do serviço.
- [ ] *Remove the service…*: os arquivos voltam para o perfil e o app volta a rodar o host.

## Terminal
- [ ] *Terminal* no cartão de um computador: abre a janela com o PowerShell (Windows) ou o shell do usuário (Linux/Mac); cores, setas, Tab, Ctrl+C e colar funcionam; programas de tela cheia (vim, htop) desenham certo.
- [ ] Redimensionar a janela: o shell acompanha (por exemplo `$Host.UI.RawUI.WindowSize` ou `tput cols`).
- [ ] Com o serviço do Windows: o shell roda como quem está logado (`whoami`), não como SYSTEM; sem ninguém logado, o terminal é recusado.
- [ ] Papel *terminal only*: abre só o terminal, sem imagem; *view only* e *files only* não abrem terminal.
- [ ] `rdesk shell "PC"` num terminal: funciona e devolve o código de saída do shell.
- [ ] A barra de aviso no host mostra "(terminal open)"; a atividade registra quem abriu.

## Extras
- [ ] Ctrl+Alt+Del (menu da sessão) num Windows com o serviço: abre a tela de segurança; na tela de bloqueio, leva à senha.
- [ ] *Terminal (admin)*: `whoami /groups` mostra "Mandatory Label\High"; num usuário que não é administrador, a recusa aparece; com *full control* desligado ou a opção do host desligada, também.
- [ ] Mac: liberar uma versão nova no Admin; o app baixa, troca o `rdesk.app` e reinicia sozinho (sem ninguém conectado).
- [ ] Wayland (GNOME e KDE): primeira conexão mostra o pedido no host; aceitar dá imagem, mouse, teclado e texto (acentos); a segunda conexão não pergunta de novo.

## Instalação
- [ ] `https://seu-servidor/download` mostra a versão liberada; o Windows baixa `rdesk@seu-servidor.exe` e o app já vem com o servidor preenchido.
- [ ] *Install rdesk on this computer*: aparece em *Configurações → Aplicativos*; o Menu Iniciar abre o rdesk de `Program Files`; *Desinstalar* por lá remove tudo e devolve as configurações do host.
- [ ] `rdesk install --only-share --key rdk_…` num PC sem ninguém logado no app: ele entra no servidor sozinho.
- [ ] Linux: `curl … /install.sh | sudo sh` instala e liga o `rdesk-update.timer`; com `--host --key`, o computador aparece no servidor.
- [ ] `install-server.sh` numa VPS limpa: servidor no ar, com as unidades de atualização.

## Atualizações
- [ ] Admin → *rdesk versions* lista as versões do GitHub; *Check now* encontra uma recém-publicada.
- [ ] Marcar um PC como *Test computer* e *Release to test computers*: só ele baixa e instala (quando ninguém está conectado); o Admin mostra a versão nova nele.
- [ ] *Release to everyone…*: os outros instalam; o app da bandeja reabre na versão nova.
- [ ] Versão que não sobe (teste: liberar uma build quebrada): o Windows volta para a anterior e não tenta de novo.
- [ ] *Update the server*: backup em `backups/`, `/api/info` mostra a versão nova; o resultado aparece no Admin.
- [ ] Linux com `rdesk-update.timer`: instala o `.deb` e o host reinicia sozinho.

## Autenticação
- [ ] Chave do 1Password: aparece a aprovação/Touch ID uma vez; conecta.
- [ ] Chave não autorizada + sem senha → mensagem clara, sem retry infinito.
- [ ] `--auth password` com `--password-ref op://…` conecta; senha errada → "wrong password".
- [ ] Após queda curta (<10 min) a reconexão **não** pede o 1Password de novo.

## Robustez (as metas do M4)
- [ ] Desligar o Wi-Fi do cliente por 20 s e religar: reconecta em ≤ 2 s após a volta.
- [ ] Trocar de Wi-Fi / tirar o cabo: overlay "Reconnecting…", volta sozinho.
- [ ] Suspender o notebook e acordar: volta sozinho.
- [ ] Linux: `sudo tc qdisc add dev eth0 root netem loss 10% delay 80ms` → imagem lenta mas sem latência crescente; `tc qdisc del dev eth0 root` volta ao normal.
- [ ] Segurar Ctrl e derrubar a conexão: ao reconectar nenhuma tecla fica presa no host.
- [ ] Conectar de um segundo cliente: o primeiro recebe "Another rdesk client connected…" e não tenta reconectar.

## Tela
- [ ] Monitor primário aparece primeiro; troca de monitor pelo menu funciona.
- [ ] Texto fica nítido ~0,3 s depois de parar de rolar.
- [ ] Monitores com escala diferente (100 %/150 %): cliques caem no lugar certo, inclusive nas bordas.
- [ ] Monitor à esquerda do primário (coordenadas negativas): cliques corretos.

## Teclado
- [ ] Layout US e ABNT2 no host: digitar `ç ã é ~ ^ ' " / ?` (teclas `IntlRo`, `IntlBackslash`, teclas mortas).
- [ ] AltGr (Windows) produz `@`, `€`, etc.
- [ ] Mac cliente: Cmd+C / Cmd+V copiam/colam no Windows (troca Cmd⇄Ctrl).
- [ ] Atalhos: Ctrl+Shift+Esc, Alt+Tab (no Linux cliente), F1–F12, setas, Home/End, numpad com NumLock.

## Clipboard e arquivos
- [ ] Copiar texto no host aparece no cliente em ≤ 1 s e vice-versa; sem efeito ping-pong.
- [ ] Arrastar um arquivo para a janela: chega em `Downloads/rdesk` do host.
- [ ] Enviar 1 GB, derrubar a conexão no meio, reenviar: retoma de onde parou e o blake3 confere.
- [ ] Navegar pastas do host pelo menu e baixar um arquivo.
- [ ] Nome malicioso (`../../x`, `CON.txt`) é sanitizado.

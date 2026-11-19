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

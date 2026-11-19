# Checklist de testes manuais

Rodar para cada par cliente → host relevante: **Mac → Windows**, **Linux → Windows** (e Mac → Mac/Linux quando houver).

## Rede
- [ ] Sem relay, na LAN: `rdesk connect <ID>@<ip-da-lan>` conecta; a barra mostra RTT `(direct)`.
- [ ] Com relay (`rdesk net relay …` nas duas pontas): conecta só com o ID, de outra rede (4G); a barra começa em `(relay)` e passa a `(direct)` quando o hole punching funciona.
- [ ] Relay fora do ar: o host mostra *Relay unreachable* e conexões diretas continuam funcionando.
- [ ] ID errado (um caractere trocado) → não conecta ("no answer"), nada é enviado.

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

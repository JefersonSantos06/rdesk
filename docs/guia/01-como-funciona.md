# 1. Como o rdesk funciona

Cinco minutos de leitura que poupam horas depois.

## As peças

```
            ┌───────────────────────────────┐
            │  SEU SERVIDOR (uma VPS)        │
            │  rdesk-server                  │
            │  • contas (Google/GitHub + 2FA)│
            │  • quem pode acessar o quê     │
            │  • relay (a "ponte")           │
            └───────▲───────────────▲────────┘
                    │ internet      │
        ┌───────────┴───┐       ┌───┴────────────┐
        │ NOTEBOOK      │       │ PC DE CASA     │
        │ app rdesk     │══════>│ app rdesk      │
        │ (cliente)     │ tela, │ (host ligado)  │
        └───────────────┘ teclado, arquivos ───┘
```

- **O servidor** fica numa VPS sua, com um domínio (ex.: `rdesk.seu-dominio.com`). Ele
  sabe **quem você é** (login com Google ou GitHub, mais um segundo fator) e **quem pode
  acessar cada computador**. Dentro dele roda o **relay**.
- **O app** é o mesmo em todo computador. Ele tem duas metades:
  - o **cliente**, que abre uma janela com a tela de outro computador;
  - o **host**, que deixa **este** computador ser acessado. O host só funciona ligado.
- Um computador pode usar só o cliente, só o host, ou os dois.

## Como uma conexão acontece

1. No notebook, você clica em **Conectar** no PC de casa.
2. O app pede ao servidor um **tíquete**: "esta pessoa pode controlar aquele computador".
   O servidor só entrega se você tiver acesso. O tíquete vale até 12 horas.
3. Os dois computadores se acham pelo relay e **tentam uma ligação direta** entre eles
   (atravessando os roteadores, o famoso *hole punching*). Quase sempre dá certo.
4. Se a rede não deixar a ligação direta, o relay **repassa os dados**. Fica um pouco mais
   lento, mas funciona. Tudo continua cifrado de ponta a ponta: o relay só vê dados
   embaralhados.
5. O PC de casa confere o tíquete sozinho (ele conhece a assinatura do servidor) e abre a
   sessão.

Você não precisa abrir portas no roteador nem configurar nada para isso. Durante a sessão,
o painel *Info* da janela mostra se a ligação está **direta** ou passando pelo **relay**.

## E se a conexão cair?

O rdesk percebe em segundos e reconecta sozinho, para sempre, até você desistir. Na
volta, ele usa um "token de retomada" (válido por 10 minutos), então você não precisa
entrar de novo. Enquanto está fora, a imagem escurece e o teclado fica pausado (nada do que
você digita é enviado depois, de surpresa).

## Três jeitos de alguém entrar num computador

| Jeito | Precisa do servidor? | De onde funciona | Quando usar |
|---|---|---|---|
| **Pelo servidor** (tíquete) | Sim | De qualquer lugar | O normal. Você controla quem acessa pelo app. |
| **Acesso direto** (senha ou chave SSH **do computador**) | Não | Só da **rede local** ou do **Tailscale** | Antes de ter servidor, ou como reserva dentro de casa. |
| **Pelo servidor + cadeado extra** | Sim | De qualquer lugar | Quando você quer, além da permissão, a senha do computador. |

"Acesso direto" não tem nada a ver com a ligação direta do passo 3: aquele é **o caminho
dos dados**; este é **quem autoriza** a entrada (o próprio computador, sem servidor).

## Contas e acesso

- Só entram no servidor os e-mails que um **admin** liberou.
- Cada computador cadastrado tem um **dono**. O dono escolhe quem acessa:
  **só eu**, **pessoas e grupos** (cada um com um nível) ou **todos do servidor**.
- Níveis de acesso:
  - **controle total**: tela, teclado e mouse, área de transferência, arquivos e terminal;
  - **só visualizar**: só a tela;
  - **só arquivos**: enviar, baixar e navegar nas pastas, sem a tela;
  - **só terminal**: um terminal, sem a tela (bom para placas sem monitor).
- O dono pode ter **gestores**: pessoas com controle total que também compartilham,
  renomeiam e veem o histórico.
- Tirar o acesso de alguém derruba na hora uma sessão que esteja aberta.

## Segurança em uma frase

O ID de cada computador é a própria chave criptográfica dele: ninguém consegue se passar
por ele. Tudo vai cifrado, o servidor nunca vê a sua tela, e a sua senha (se usar uma) nem
sai do seu computador. Detalhes em [Segurança](07-seguranca.md).

## Limites que é bom saber

- **Uma sessão por vez** em cada computador: se outra pessoa conectar, a sua cai.
- **Sem áudio.** O cursor do outro computador não aparece na imagem (use o seu).
- A **área de transferência** passa só **texto** (copiar e colar nos dois sentidos), e só
  com controle total.
- **Arquivos** recebidos vão para `Downloads/rdesk` do outro computador. Uma transferência
  interrompida continua de onde parou.
- **Linux**: o host precisa da sessão gráfica **X11**. O Wayland funciona pelo "portal" do
  sistema (alguém no computador precisa permitir na primeira vez), mas ainda é novo.
- **Windows**: tela de bloqueio, janelas de administrador (UAC) e Ctrl+Alt+Del só com o
  **serviço do Windows** instalado.

Próximo: [Criar o servidor](02-servidor.md) ou, se já tem um, [Instalar o app](03-instalar-o-app.md).

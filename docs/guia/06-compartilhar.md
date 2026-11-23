# 6. Compartilhar

Todo computador cadastrado tem um **dono** (quem cadastrou, ou quem aprovou a entrada
dele). O dono decide quem mais pode acessar.

## Gerenciar acesso

1. Em **Computadores**, selecione o computador.
2. No painel, em **Quem tem acesso**, clique em **Gerenciar acesso**.
3. Escolha uma das três opções:

| Opção | Quem acessa |
|---|---|
| **Só eu** | você e os seus gestores. A lista de pessoas e grupos fica guardada, mas não vale. |
| **Pessoas e grupos** | você, os gestores e quem estiver na lista, cada um com o seu nível. |
| **Todos do servidor** | todo mundo com conta no servidor, com controle total. |

4. Em **Pessoas e grupos**, digite no campo **Adicionar pessoa ou grupo** e clique no
   resultado (só aparecem pessoas com conta no servidor e grupos que existem). Para cada um,
   escolha o nível no menu ao lado:
   - **controle total**: tela, teclado e mouse, área de transferência, arquivos e terminal;
   - **só visualizar**: só ver a tela;
   - **só arquivos**: enviar, baixar e navegar nas pastas, sem a tela;
   - **só terminal**: um terminal, sem a tela;
   - **controle total e pode gerenciar** (só o dono vê esta opção): vira **gestor**.

   O **X** tira a pessoa ou o grupo da lista.
5. **Salvar**. Se a mudança tira acesso de alguém, o diálogo avisa antes.

Tirar ou diminuir o acesso de alguém vale **na hora**: se a pessoa estiver conectada, a
sessão cai.

Quem está em vários lugares (por exemplo, numa pessoa e num grupo) fica com a soma das
permissões.

## Gestores

Gestores são pessoas de confiança do dono. Eles têm **controle total** e também podem
compartilhar, renomear e ver o histórico. Só o **dono** escolhe os gestores (opção
*controle total e pode gerenciar*) e só ele pode **descadastrar** o computador.

## Grupos

Em vez de adicionar pessoa por pessoa em cada computador, crie um grupo (ex.: "Família",
"Suporte") e compartilhe com ele.

1. **Grupos** (barra lateral) → **Novo grupo** → nome → **Criar**. Quem cria vira
   **admin do grupo**.
2. Selecione o grupo e abra a aba **Membros**. Em **Adicionar uma pessoa**, digite o nome ou
   e-mail e clique em **Adicionar** (marque *como admin do grupo* se for o caso).
3. No **⋯** de cada membro: **Tornar admin** / **Tornar membro** e **Remover do grupo…**.
4. Para renomear, clique no lápis ao lado do nome do grupo.
5. **Sair do grupo** e **Excluir grupo…** ficam no fim da página.

Depois, compartilhe os computadores com o grupo em **Gerenciar acesso**. Quem entra no
grupo ganha o acesso; quem sai perde na hora.

A aba **Computadores** do grupo (a primeira) mostra tudo o que está compartilhado com ele,
num lugar só: quem está online, de quem é cada um e o que o grupo pode fazer lá (*controle
total*, *só visualizar*, *só arquivos*, *só terminal*). **Conectar** e o botão de terminal
abrem a sessão dali mesmo. Um computador marcado **privado** guarda o compartilhamento, mas
o grupo não acessa enquanto ele estiver em **Só eu**; só quem o gerencia o vê nessa lista.

Excluir um grupo tira o acesso que ele dava aos membros.

## Aprovar computadores que pediram para entrar

Quando alguém instala o rdesk num computador com o **seu e-mail** como dono
([cenário D](04-cenarios.md#d-só-ser-acessado-sem-conta-nele)), aparece no topo de
**Computadores** o aviso **"N computadores aguardam sua aprovação"**.

1. Clique em **Revisar**.
2. Para cada pedido aparecem o nome, o sistema, o **código** e quando foi pedido. Confira
   com quem está no computador se o código é o mesmo que aparece lá.
3. **Aprovar…** → escolha o nome e quem pode ver → **Aprovar**. O computador vira seu e
   aparece online em segundos.
4. Ou **Recusar…**: ele fica recusado por 30 dias.

Admins do servidor também veem os pedidos feitos **sem** e-mail de dono.

## Histórico

Selecione um computador seu (ou de que você é gestor): o painel mostra as **sessões
recentes**. **Histórico completo** mostra quem acessou, quando, por quanto tempo e com
qual nível. Os admins do servidor também podem ver.

## Renomear e descadastrar

- **Renomear**: o lápis ao lado do nome no painel (ou **F2**).
- **Descadastrar** (só o dono): no painel, **Avançado → Descadastrar do servidor…**. O
  computador sai do servidor: ninguém mais chega a ele por lá, e o compartilhamento se
  perde. Para voltar, cadastre de novo pelo próprio computador.

## Quem vê com quem está compartilhado

Todos que podem acessar um computador veem, no painel, o dono, os gestores e com quem
ele está compartilhado. Só o dono e os gestores mudam isso.

## No próprio computador

O computador em que o app está aberto aparece sempre **no topo** da lista, com o selo
**Este computador**, mesmo que outra conta o tenha cadastrado e não o tenha compartilhado
com você. Ali você vê o dono e com quem ele está compartilhado (também na página
**Este computador**). Também pode **gerenciá-lo**: compartilhar com pessoas ou grupos,
renomear, ver o histórico e descadastrar. Antes, o sistema confirma um administrador:
no Windows o rdesk abre de novo como administrador, e no macOS e no Linux o sistema pede a
senha de um administrador.

Dele mesmo não se abre a **tela** nem os **arquivos** (seria a tela mostrando a si mesma).
O **terminal** continua disponível, se o seu acesso permitir. Se ele ainda não estiver
cadastrado, o topo da lista oferece **Cadastrar…**.

## Computadores salvos neste aparelho ("Nesta rede")

Os computadores da rede local ([cenário F](04-cenarios.md#f-sem-servidor-rede-local-ou-tailscale))
ficam só no seu aparelho, não no servidor: não dá para compartilhá-los. Cada pessoa adiciona
os seus. No painel deles há **Editar…** e **Remover…**.

# 7. Segurança

O rdesk foi feito para não depender de você "fazer tudo certo". Mesmo assim, alguns
cuidados valem a pena.

## O que já vem protegido

- **Tudo cifrado** de ponta a ponta. O servidor e o relay só veem dados embaralhados.
- O **ID do host** é a própria chave criptográfica do computador: ninguém consegue se passar
  por ele, e ninguém cadastra o ID de outra máquina como sua.
- Para entrar num computador cadastrado é preciso um **tíquete** do servidor, que só sai
  para quem tem acesso e vale no máximo 12 horas. O computador confere sozinho.
- Tirar o acesso de alguém (ou desativar a conta) derruba na hora uma sessão aberta.
- **Quem está na frente da tela sempre sabe** quando alguém está conectado e pode encerrar
  (no Windows, a barra no alto da tela com **Encerrar**).
- Senhas próprias de computador são guardadas de um jeito que nem o computador conhece a
  senha (OPAQUE + Argon2id).
- Muitas tentativas erradas de login atrasam as próximas.

## O seu login (segundo fator)

- O login no servidor pede **Google ou GitHub** e um **segundo fator** em **toda** entrada:
  um app autenticador (TOTP) ou uma passkey.
- **Guarde bem o segundo fator.** Uma boa prática é guardar o TOTP no 1Password (que
  funciona em vários aparelhos).
- **Perdeu o celular** (ou quer trocar de app/passkey)? Peça a um admin para **redefinir o
  segundo fator** ([como](08-administrar.md#perdeu-o-segundo-fator)). As suas sessões são
  encerradas e, no próximo login com Google ou GitHub, você cadastra um novo.
- O login vale 10 dias. Para sair antes: *Configurações → Conta → Sair*.
- Saiu do emprego, perdeu o notebook? Um admin pode **desativar** a conta em
  *Admin → Usuários* (encerra as sessões e revoga os acessos na hora).

## Senha e chaves SSH próprias do computador

Cada computador pode ter uma **senha** e/ou **chaves SSH** próprias, em
**Este computador → Segurança**. Elas servem para duas coisas:

1. **Acesso direto** (sem servidor), na rede local ou pelo Tailscale
   ([cenário F](04-cenarios.md#f-sem-servidor-rede-local-ou-tailscale));
2. **Cadeado extra** (abaixo).

**Senha**: **Definir senha…** → **Gerar uma forte** → **Copiar** (guarde no gerenciador de
senhas) → **Salvar senha**. Também dá para ler direto do 1Password, com uma referência
`op://Cofre/item/campo`. Prefira 16 caracteres ou mais.

**Chave SSH** (mais prática com o 1Password): **Adicionar chave SSH…** e escolha uma chave
do agente SSH do 1Password, ou cole a chave pública de quem vai acessar
(`ssh-ed25519 AAAA…`). Quem tem a chave privada entra (no 1Password, ele pede a sua
aprovação ou digital).

Para pegar a **sua** chave pública e autorizá-la em outro computador:
*Configurações → Chaves SSH → Carregar do agente → Copiar chave pública*.

> Para usar o agente SSH do 1Password, ligue-o em 1Password → *Configurações →
> Desenvolvedor → Usar o agente SSH*.

## Cadeado extra

Com o **cadeado extra** ligado, para entrar no computador é preciso **a permissão do
servidor e também** a senha ou chave dele. Ligue em **Este computador → Segurança →
Cadeado extra** (precisa ter definido uma senha ou chave antes) e **Aplicar**.

Do lado de quem acessa, o app pergunta a senha ou usa a chave. Para não perguntar toda vez:
selecione o computador → **⋯ → Opções de login…** e escolha:

- **Chave do 1Password** (com a impressão digital da chave, ou **Do 1Password…** para
  escolher);
- uma **referência do 1Password** (`op://…`) para a senha;
- ou digite a senha em **Senha salva** para guardá-la no cofre do sistema (Gerenciador de
  Credenciais do Windows, Chaves do Mac, Secret Service no Linux).

## Acesso direto num computador cadastrado

Num computador cadastrado, o acesso direto vem **desligado**. Ligá-lo abre uma segunda
porta (a senha/chave dele, só da rede local ou Tailscale). Quem tem acesso pelo servidor vê
o aviso de que ele está ligado, e essas sessões entram no histórico. Ligue só se precisar
([cenário G](04-cenarios.md#g-servidor-e-rede-local-juntos-acesso-direto)).

## Se um computador for roubado ou comprometido

- No seu app, selecione-o → **Avançado → Descadastrar do servidor…**. Ninguém mais entra
  nele pelo servidor.
- Se ele tinha uma chave SSH sua autorizada, nada a fazer: a chave **privada** continua só
  com você.
- **Nova identidade do host** (*Este computador → Segurança*) troca o ID do computador
  (quem o tinha salvo pelo ID precisa do novo).

## Boas práticas, em resumo

1. Segundo fator guardado num lugar seguro (1Password).
2. Compartilhe com **grupos** e com o **menor nível** que resolve (só visualizar, só
   arquivos).
3. Deixe o **acesso direto desligado** nos computadores cadastrados, a não ser que precise.
4. Para computadores sensíveis, ligue o **cadeado extra**.
5. Revogue **chaves de cadastro** quando terminar de usá-las.
6. Confira de vez em quando o **histórico** dos seus computadores e a aba **Atividade** de
   *Este computador* (logins que falharam).

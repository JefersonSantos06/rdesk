# Guia do rdesk

Este guia explica, passo a passo, como montar e usar o rdesk: do servidor na nuvem até
acessar o computador de casa pelo notebook do trabalho. Não precisa saber programar. Siga
os passos na ordem e copie os comandos como estão.

O rdesk roda em **Windows**, **Mac** (Apple Silicon) e **Linux**. No **celular** (Android e iPhone) há um app que acessa os computadores (o celular não é acessado): veja [O app do celular](12-celular.md).

## Por onde começar

Responda às perguntas e vá direto ao capítulo certo.

**1. Você quer acessar computadores pela internet (de qualquer lugar)?**

- **Sim** → você precisa de um **servidor rdesk** (uma vez só, para todos os seus
  computadores). Comece por [Criar o servidor](02-servidor.md). Se alguém já tem um
  servidor e liberou o seu e-mail, pule para [Instalar o app](03-instalar-o-app.md).
- **Não, só dentro de casa / do escritório** (mesma rede, ou pelo Tailscale) → não precisa
  de servidor. Vá para [Sem servidor](04-cenarios.md#f-sem-servidor-rede-local-ou-tailscale).

**2. O que cada computador vai fazer?**

| Este computador vai… | Cenário |
|---|---|
| acessar outros **e** ser acessado (o seu PC, o seu notebook) | [A. Usar e ser acessado](04-cenarios.md#a-usar-e-ser-acessado-cliente--host) |
| só acessar outros (o notebook do trabalho) | [B. Só acessar](04-cenarios.md#b-só-acessar-outros-só-cliente) |
| só ser acessado, e você entra com a sua conta nele (o PC de casa) | [C. Só ser acessado, com a sua conta](04-cenarios.md#c-só-ser-acessado-com-a-sua-conta) |
| só ser acessado, **sem** entrar com conta nele (o PC da vó, de um cliente) | [D. Só ser acessado, sem conta](04-cenarios.md#d-só-ser-acessado-sem-conta-nele) |
| vários de uma vez, iguais (Raspberry Pi, máquinas de uma sala) | [E. Vários computadores de uma vez](04-cenarios.md#e-vários-computadores-de-uma-vez-chave-de-cadastro) |
| conversar só na rede local, sem servidor | [F. Sem servidor](04-cenarios.md#f-sem-servidor-rede-local-ou-tailscale) |
| ser acessado pelo servidor **e** direto na rede local | [G. Servidor e rede local juntos](04-cenarios.md#g-servidor-e-rede-local-juntos-acesso-direto) |

## Capítulos

1. [Como o rdesk funciona](01-como-funciona.md): as peças (servidor, relay, app, host) em
   linguagem simples. Leia uma vez: o resto do guia fica bem mais fácil.
2. [Criar o servidor](02-servidor.md): a VPS, o domínio, o login com Google/GitHub, o
   primeiro acesso.
3. [Instalar o app](03-instalar-o-app.md): Windows, Mac e Linux, e o primeiro login.
4. [Cenários de uso](04-cenarios.md): a receita completa de cada caso da tabela acima.
5. [Usar no dia a dia](05-usar.md): conectar, a janela da sessão, arquivos, terminal,
   teclas, qualidade, reconexão.
6. [Compartilhar](06-compartilhar.md): quem pode acessar o quê, grupos, gestores,
   histórico, aprovar computadores.
7. [Segurança](07-seguranca.md): segundo fator, senha e chaves SSH próprias, cadeado extra,
   1Password.
8. [Administrar](08-administrar.md): liberar e-mails, atualizações, backup do servidor.
9. [Linha de comando](09-linha-de-comando.md): todos os comandos do `rdesk` e do
   `rdesk-server`.
10. [Problemas e soluções](10-problemas.md): não conecta, tela preta, cliques que não
    funcionam, onde ficam os logs.
11. [Desinstalar](11-desinstalar.md): tirar o app, o serviço e o servidor.
12. [O app do celular](12-celular.md): Android e iPhone, gestos, teclado e a chave do celular.

## Palavras que aparecem o tempo todo

| Palavra | O que é |
|---|---|
| **Servidor** | O programa `rdesk-server`, numa VPS sua. Guarda as contas e a lista de computadores, decide quem pode acessar o quê e serve de **relay**. |
| **Relay** | A "ponte" dentro do servidor. Ajuda dois computadores a se acharem e, quando a rede não deixa uma ligação direta, repassa os dados (sempre cifrados: o servidor não vê nada). |
| **App** | O programa `rdesk` que você instala em cada computador. Ele é ao mesmo tempo **cliente** e **host**. |
| **Cliente** | A parte do app que **acessa** outros computadores (abre a janela com a tela do outro). |
| **Host** | A parte do app que **deixa este computador ser acessado**. Liga e desliga em *Este computador*. |
| **ID do host** | O "endereço" único de cada computador (64 letras e números). Não muda, a não ser que você peça uma identidade nova. |
| **Cadastrar** | Registrar um computador no servidor. Cadastrado, ele aparece para quem tem acesso e fica acessível mesmo sem ninguém logado no app. |
| **Chave de cadastro** | Um código `rdk_…` que cadastra computadores na hora, sem aprovação. |
| **Acesso direto** | Entrar num computador com a senha ou chave SSH **dele**, sem o servidor, só da rede local ou do Tailscale. |
| **Cadeado extra** | Pedir também a senha ou chave do computador, além da permissão do servidor. |
| **Serviço do Windows** | O host rodando como serviço: o computador fica acessível antes de alguém entrar no Windows, na tela de bloqueio e nas janelas de administrador (UAC). |

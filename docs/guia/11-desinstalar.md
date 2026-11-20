# 11. Desinstalar

**Antes**: se o computador não vai mais ser usado com o rdesk, **descadastre-o** do
servidor (no app de qualquer computador seu: selecione-o → **Avançado → Descadastrar do
servidor…**). Assim ele some da lista de todo mundo.

## Windows

**Pelo Windows**: *Configurações → Aplicativos → Aplicativos instalados* → **rdesk** →
**Desinstalar**.

Ou, num PowerShell:

```powershell
& "C:\Program Files\rdesk\rdesk.exe" uninstall
```

Isso remove o serviço, a regra do firewall, o atalho do Menu Iniciar, o início automático,
a entrada em *Aplicativos* e a pasta `C:\Program Files\rdesk`. A identidade do host
(o ID) volta para a sua pasta de usuário, caso você reinstale.

Para apagar **tudo** (inclusive o login, as configurações e o ID), apague também a pasta
`%APPDATA%\rdesk` (cole no Explorador de Arquivos).

> **Versões antigas (0.3.x)** não têm o `uninstall`. Feche o app (bandeja → *Sair*) e, num
> PowerShell **como administrador**:
>
> ```powershell
> & "C:\Program Files\rdesk\rdesk.exe" service uninstall
> Remove-Item -Recurse -Force "C:\Program Files\rdesk"
> ```
>
> Se houver uma cópia em `%LOCALAPPDATA%\Programs\rdesk`, apague essa pasta também.

## Mac

1. Desligue o início automático: *Configurações → Geral → Iniciar com o sistema* →
   **Não iniciar automaticamente** (ou, no Terminal,
   `/Applications/rdesk.app/Contents/MacOS/rdesk autostart off`).
2. Saia do rdesk (ícone na barra de menus → **Sair do rdesk**).
3. Arraste o **rdesk.app** de Aplicativos para o **Lixo**.
4. Para apagar tudo: a pasta `~/Library/Application Support/dev.rdesk.rdesk/` (no Finder:
   *Ir → Ir para a Pasta…*).
5. Em *Ajustes do Sistema → Privacidade e Segurança*, tire o rdesk de **Gravação de Tela**
   e **Acessibilidade**.

## Linux

```sh
systemctl --user disable --now rdesk-host 2>/dev/null   # se era um computador só acessado
sudo systemctl disable --now rdesk-update.timer
sudo apt remove rdesk
sudo rm -f /etc/default/rdesk-update
```

Para apagar tudo (login, configurações, ID, início automático):

```sh
rm -rf ~/.config/rdesk ~/.config/rdesk-enroll.env ~/.config/autostart/rdesk.desktop
```

## Servidor

Na VPS (isso apaga **todas** as contas e computadores do servidor; faça um
[backup](08-administrar.md#backup-do-servidor) antes, se quiser guardar):

```sh
sudo systemctl disable --now rdesk-server rdesk-server-update.path
sudo rm -f /etc/systemd/system/rdesk-server.service /etc/systemd/system/rdesk-server-update.path /etc/systemd/system/rdesk-server-update.service
sudo systemctl daemon-reload
sudo rm -f /usr/local/bin/rdesk-server
sudo rm -rf /var/lib/rdesk-server
sudo userdel rdesk
```

Depois, apague os apps OAuth no Google Cloud e no GitHub, e o registro DNS do domínio.

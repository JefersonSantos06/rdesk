# Publicar uma versão

Cada versão é um GitHub Release com os binários e um `release.json` assinado. O app confere essa assinatura com chaves embutidas nele, então ninguém consegue entregar um binário alterado: nem quem tomar a VPS, nem o GitHub, nem o caminho do download.

## O que cada release tem

| Arquivo | O que é |
|---|---|
| `rdesk-<versão>-windows-x86_64.zip` | O app (cliente + host) para Windows |
| `rdesk-<versão>-linux-x86_64.tar.gz`, `…-linux-aarch64.tar.gz` | O app para Linux (PC e Raspberry/Orange Pi) |
| `rdesk_<versão>_amd64.deb`, `…_arm64.deb` | O app como pacote Debian/Ubuntu/Raspberry Pi OS |
| `rdesk-host_<versão>_amd64.deb`, `…_arm64.deb` | Só o host, para servidores sem tela (terminal, arquivos e túneis; sem o app e sem bibliotecas de área de trabalho); atualiza para o próprio pacote (plataformas `linux-x86_64-host`, `linux-aarch64-host` no `release.json`) |
| `rdesk-<versão>-macos-aarch64.tar.gz` | O `rdesk.app` para Mac com Apple Silicon (assinado com o certificado próprio do rdesk, não o da Apple: na primeira vez, o macOS 15 em diante só abre por *Ajustes → Privacidade e Segurança → Abrir mesmo assim*) |
| `rdesk-<versão>-android.apk` | O app do celular para Android (assinado com a chave de release do Android) |
| `rdesk-<versão>-ios.ipa` | O app para iPhone, **sem assinatura**: o Sideloadly ou o AltStore assinam com o seu Apple ID ao instalar ([mobile/README.md](../mobile/README.md)) |
| `rdesk-server-<versão>-linux-x86_64.tar.gz`, `…-linux-aarch64.tar.gz` | O servidor, com a unit do systemd |
| `release.json` | Versão, versão do protocolo, SHA-256 e tamanho de cada arquivo |
| `release.json.sig` | Assinatura Ed25519 do `release.json` (hex) |

O app para Linux (x86_64 e ARM64) é compilado no Ubuntu 22.04, o Linux mais antigo que ele atende (glibc 2.35, PipeWire 0.3), e roda no Ubuntu 22.04 ou mais novo, no Debian 12 ou mais novo e no Raspberry Pi OS (bookworm e trixie); o release falha se o programa passar a pedir uma glibc mais nova que a 2.35. O crate `libspa` pede cabeçalhos do PipeWire 0.3.65 ou mais novos, e o 22.04 tem os do 0.3.48: `.github/scripts/pipewire-headers.sh` baixa só os cabeçalhos do Debian 12 (com SHA-256 fixo), e o programa continua ligado à `libpipewire` do próprio 22.04, então uma função mais nova que a 0.3.48 quebra a compilação, não a abertura do app. O servidor também é compilado no Ubuntu 22.04 e roda em VPS com Debian 12. O app para Mac (Apple Silicon, macOS 11+) não tem assinatura da Apple: na primeira vez, botão direito → *Abrir* até o macOS 14, e *Ajustes → Privacidade e Segurança → Abrir mesmo assim* do 15 em diante (ou `xattr -dr com.apple.quarantine rdesk.app`); como host, o macOS pede Gravação de Tela e Acessibilidade. O instalador do Windows (Inno Setup) entra na fase 9.

## Configuração (uma vez)

### As chaves de assinatura

Foram geradas duas, em `C:\Users\Jeferson\rdesk-release-keys\` (fora do repositório):

- `rdesk-release-main.key`: a que o CI usa.
- `rdesk-release-reserve.key`: reserva, **nunca vai para o GitHub**. Serve para trocar a principal se ela vazar.

As chaves públicas estão em `crates/proto/src/release.rs` (`RELEASE_KEYS`).

1. Guarde as duas chaves privadas no 1Password (o conteúdo de cada `.key`).
2. No GitHub: *Settings → Environments → New environment* `release`. Em *Environment secrets*, crie `RDESK_RELEASE_KEY` com o conteúdo de `rdesk-release-main.key`.
   - Opcional: em *Required reviewers*, coloque você. Cada release passa a esperar a sua aprovação antes de assinar e publicar.
3. Apague a pasta `rdesk-release-keys` do disco.

Para gerar chaves novas no futuro: `cargo run -p rdesk-release -- keygen --out <pasta> --name <nome>`.

### A chave do APK (Android)

O APK tem a sua própria assinatura, a do Android, além do `release.json`. Precisa ser sempre a mesma chave, senão o celular recusa a atualização. Como criar e quais *Repository secrets* configurar (`RDESK_ANDROID_KEYSTORE_B64` e as senhas): [mobile/README.md](../mobile/README.md#a-chave-de-assinatura-do-android). Sem eles, o job do Android falha e o Release não é publicado.

### O certificado do Mac

O macOS guarda as permissões do rdesk (Gravação de Tela, Acessibilidade) pela assinatura do app. Assinado *ad hoc*, cada versão tem uma assinatura diferente, e depois de cada atualização o Mac pede as permissões de novo. Com um certificado próprio, sempre o mesmo, elas continuam valendo. O certificado é autoassinado: não precisa de conta da Apple, e o aviso do Gatekeeper na primeira abertura continua.

Crie uma vez, no PowerShell (o `openssl` vem com o Git para Windows). O segundo comando pede a senha do `.p12`:

```powershell
& "C:\Program Files\Git\usr\bin\openssl.exe" req -x509 -newkey rsa:2048 -nodes -days 36500 -subj "/CN=rdesk" -keyout "$HOME\rdesk-macos.key" -out "$HOME\rdesk-macos.crt" -addext "basicConstraints=critical,CA:FALSE" -addext "keyUsage=critical,digitalSignature" -addext "extendedKeyUsage=critical,codeSigning"
```

```powershell
& "C:\Program Files\Git\usr\bin\openssl.exe" pkcs12 -export -name rdesk -inkey "$HOME\rdesk-macos.key" -in "$HOME\rdesk-macos.crt" -out "$HOME\rdesk-macos.p12" -keypbe PBE-SHA1-3DES -certpbe PBE-SHA1-3DES -macalg sha1
```

Guarde o `rdesk-macos.p12` e a senha no 1Password e crie os *Repository secrets*:

```powershell
[Convert]::ToBase64String([IO.File]::ReadAllBytes("$HOME\rdesk-macos.p12")) | gh secret set RDESK_MACOS_CERT_P12_B64 -R JefersonSantos06/rdesk
```

```powershell
gh secret set RDESK_MACOS_CERT_PASSWORD -R JefersonSantos06/rdesk
```

Depois apague os três arquivos (`rdesk-macos.key`, `.crt`, `.p12`) do disco. Sem os secrets (ou se a assinatura falhar), o Release assina *ad hoc* e avisa na execução. Trocar o certificado tem o mesmo efeito de uma assinatura *ad hoc*: os Macs pedem as permissões mais uma vez.

### Trocar a chave principal

1. Gere uma chave nova e coloque a pública em `RELEASE_KEYS`, no lugar da antiga (a reserva continua).
2. Publique essa versão assinada com a **reserva** (troque o secret temporariamente). Os apps instalados confiam na reserva, então aceitam a versão que traz a chave nova.
3. Volte o secret para a chave nova.

## Publicar

1. Suba a versão em `Cargo.toml` (`[workspace.package] version`, sempre `MAJOR.MINOR.PATCH`) e faça o commit.
2. Crie a tag igual à versão e envie. O commit marcado **não** pode ter `[skip ci]` na mensagem: o GitHub também deixa de rodar o Release da tag.

   ```sh
   git tag v0.2.0
   git push origin v0.2.0
   ```

3. O workflow *Release* (`.github/workflows/release.yml`), que também gera o `rdesk.app` do Mac:
   - confere se a tag bate com a versão do `Cargo.toml`;
   - roda os testes e compila Windows x64, Linux x64 e Linux arm64;
   - gera os `.zip`, `.tar.gz` e `.deb`;
   - gera o app do celular: o `.apk` (Ubuntu) e o `.ipa` sem assinatura (macOS);
   - escreve o `release.json`, assina, confere a assinatura com as chaves embutidas no app e publica o Release com notas geradas a partir dos commits.

Os runners ARM e os minutos de CI são gratuitos com o repositório público; com ele privado, o job `linux-aarch64` pode não ter runner.

## Conferir à mão

Com os arquivos de um release numa pasta:

```sh
cargo run -p rdesk-release -- verify --dir <pasta>
```

# Publicar uma versão

Cada versão é um GitHub Release com os binários e um `release.json` assinado. O app confere essa assinatura com chaves embutidas nele, então ninguém consegue entregar um binário alterado: nem quem tomar a VPS, nem o GitHub, nem o caminho do download.

## O que cada release tem

| Arquivo | O que é |
|---|---|
| `rdesk-<versão>-windows-x86_64.zip` | O app (cliente + host) para Windows |
| `rdesk-<versão>-linux-x86_64.tar.gz`, `…-linux-aarch64.tar.gz` | O app para Linux (PC e Raspberry/Orange Pi) |
| `rdesk_<versão>_amd64.deb`, `…_arm64.deb` | O app como pacote Debian/Ubuntu/Raspberry Pi OS |
| `rdesk-server-<versão>-linux-x86_64.tar.gz`, `…-linux-aarch64.tar.gz` | O servidor, com a unit do systemd |
| `release.json` | Versão, versão do protocolo, SHA-256 e tamanho de cada arquivo |
| `release.json.sig` | Assinatura Ed25519 do `release.json` (hex) |

O app para Linux é compilado no Ubuntu 24.04 (precisa do PipeWire 1.x, para a captura no Wayland) e roda no Ubuntu 24.04 ou mais novo, no Debian 13 e no Raspberry Pi OS atual (trixie); no Debian 12 e no Ubuntu 22.04, compile a partir do código. O servidor é compilado no Ubuntu 22.04 e também roda em VPS com Debian 12. O instalador do Windows (Inno Setup) entra na fase 9; o macOS continua só no CI.

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

### Trocar a chave principal

1. Gere uma chave nova e coloque a pública em `RELEASE_KEYS`, no lugar da antiga (a reserva continua).
2. Publique essa versão assinada com a **reserva** (troque o secret temporariamente). Os apps instalados confiam na reserva, então aceitam a versão que traz a chave nova.
3. Volte o secret para a chave nova.

## Publicar

1. Suba a versão em `Cargo.toml` (`[workspace.package] version`, sempre `MAJOR.MINOR.PATCH`) e faça o commit.
2. Crie a tag igual à versão e envie:

   ```sh
   git tag v0.2.0
   git push origin v0.2.0
   ```

3. O workflow *Release* (`.github/workflows/release.yml`):
   - confere se a tag bate com a versão do `Cargo.toml`;
   - roda os testes e compila Windows x64, Linux x64 e Linux arm64;
   - gera os `.zip`, `.tar.gz` e `.deb`;
   - escreve o `release.json`, assina, confere a assinatura com as chaves embutidas no app e publica o Release com notas geradas a partir dos commits.

Os runners ARM e os minutos de CI são gratuitos com o repositório público; com ele privado, o job `linux-aarch64` pode não ter runner.

## Conferir à mão

Com os arquivos de um release numa pasta:

```sh
cargo run -p rdesk-release -- verify --dir <pasta>
```

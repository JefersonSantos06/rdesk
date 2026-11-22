# rdesk no celular (Android e iPhone)

O app do celular **acessa** computadores. No iPhone ele não pode ser acessado (o iOS não permite). No Android, o celular também pode ser acessado, em **fase de teste** (*Configurações → Este celular*): o host do rdesk roda dentro do app e envia a tela, capturada pela MediaProjection num serviço em primeiro plano (`ScreenCaptureService.kt` → JNI → `crates/mobile/src/screen.rs`). O movimento vai em H.264 feito pelo codificador de hardware do celular (MediaCodec, `crates/host/src/h264.rs`). Por enquanto é só visualização; os toques (serviço de acessibilidade) vêm na próxima fase.

- **Flutter** desenha a interface (`lib/`).
- **Rust** faz todo o resto, com o mesmo código do app desktop: conexão iroh/QUIC, login no servidor, autenticação no computador, decodificação da imagem, arquivos e terminal.
  - `crates/core` é o núcleo do cliente, sem interface.
  - `crates/mobile` é a API que o Flutter chama, via [flutter_rust_bridge](https://cjycode.com/flutter_rust_bridge/).
- **Cargokit** (`rust_builder/`) compila o Rust dentro do `flutter build`: o Gradle no Android, o CocoaPods no iOS.

## O que ele faz

- **Login no servidor pelo navegador do celular**: Google/GitHub e o segundo fator (TOTP ou passkey). O servidor devolve o login ao app pelo endereço `rdesk://callback`.
- **Lista de computadores**:
  - os do servidor (status, dono, o que você pode fazer em cada um);
  - os salvos no celular para a rede local ou o Tailscale (sem servidor).
- **Sessão**:
  - **Imagem**: o host reduz a imagem para o tamanho da tela do celular, o que economiza dados móveis. Em *Qualidade → Resolução: completa* você pede a imagem inteira.
  - **Toque, modo touchpad** (o padrão), como o touchpad de um notebook:
    - deslizar move o ponteiro, com aceleração (devagar é preciso, rápido vai longe);
    - tocar clica; dois toques são um clique duplo;
    - tocar, tocar de novo e deslizar arrasta;
    - tocar e segurar é o botão direito;
    - dois dedos deslizando rolam, com inércia.
  - **Toque, modo toque direto**, como uma tela sensível ao toque:
    - tocar clica ali;
    - deslizar um dedo rola;
    - tocar e segurar mostra uma lupa; deslizar em seguida arrasta, soltar é o botão direito;
    - dois dedos movem a vista ampliada.
  - **Nos dois modos**: pinça dá zoom; dois dedos tocando são o botão direito; dois toques com dois dedos ampliam ali ou voltam; três dedos abrem o teclado.
  - Os gestos ficam em `lib/src/touch.dart`, sem interface e com testes (`flutter test`).
  - **Mouse**: um mouse ligado no celular aponta direto, como no computador.
  - **Ferramentas**: um botão flutuante, que se arrasta para qualquer lado, abre a folha de ferramentas (teclado, teclas, modo, gestos, tela, qualidade, girar, atalhos, área de transferência, arquivos, terminal, desconectar).
  - **Ponteiro**: o celular desenha o ponteiro do computador, cuja posição o host informa.
  - **Teclado**: o teclado do celular (acentos inclusive). Sobre ele aparece uma barra com:
    - Ctrl/Alt/Shift/Win fixáveis, Esc, Tab e setas;
    - copiar, colar, recortar, desfazer e selecionar tudo (com ⌘ quando o computador é um Mac);
    - Home/End, PgUp/PgDn, Del e F1–F12.
  - **Teclado físico**: um teclado Bluetooth ou USB-C também funciona.
  - **Área de transferência**: o que o computador copia vai para o celular; a ferramenta *Enviar copiado* manda a do celular ao computador.
  - **Arquivos**: navegar, baixar (depois compartilhar ou abrir no celular) e enviar.
  - **Terminal**: abas de terminal, com uma barra de teclas.
  - **Reconexão**: ao voltar para o app, a sessão reconecta na hora com o token de retomada, sem pedir senha de novo.
- **Cadeado extra**:
  - por **senha**, com a opção de lembrar no celular;
  - ou pela **chave deste celular**: ECDSA P-256 no hardware seguro (Android Keystore/StrongBox, Secure Enclave do iPhone), liberada com digital ou rosto. Em *Configurações → Chave deste celular*, crie a chave e copie a linha `ecdsa-sha2-nistp256 …` para o computador (*Este computador → Segurança → Chaves SSH*, ou `rdesk host add-key "…"`).
- **Atualizações (Android)**: o app avisa quando o seu servidor libera uma versão nova, baixa o APK, confere o SHA-256 contra o `release.json` assinado e abre o instalador do Android.

## Limitações

- **Segundo plano**: o celular corta a rede de apps em segundo plano. A sessão cai quando você sai do app e volta sozinha quando você entra de novo (em até 10 minutos, sem pedir a senha). Um download longo não continua com o app fechado.
- **Codec**: a imagem usa os tiles JPEG/zstd do rdesk, não vídeo H.264. Em dados móveis, use *Qualidade → Velocidade* para conteúdo que se mexe muito (vídeo, jogos).
- **Rede local sem servidor**:
  - **Android**: funciona, inclusive achar o computador pelo mDNS.
  - **iPhone**: o mDNS precisa de uma permissão especial da Apple (multicast) que um app instalado por fora da loja não tem, então salve o computador com o IP ou o nome do Tailscale.
- **iPhone sem loja**: veja *Instalar* abaixo. O app não é notarizado nem publicado.
- **Clipboard do iPhone**: o iOS mostra um aviso quando o app lê a área de transferência, então o envio é pelo botão, não automático.

## Instalar

### Android

1. Baixe `rdesk.apk` em `https://seu-servidor/download` (ou nos Releases).
2. Abra o arquivo e permita "instalar apps desta fonte" quando o Android pedir.
3. As versões seguintes chegam pelo próprio app (aviso no topo da lista).

### iPhone (sem App Store)

Baixe `rdesk.ipa` na mesma página e instale com um computador:

- **[Sideloadly](https://sideloadly.io)** (Windows ou Mac): conecte o iPhone e arraste o `.ipa`. Ele assina com o seu Apple ID.
- **[AltStore](https://altstore.io)**: faz o mesmo e renova sozinho pelo Wi-Fi.

O prazo depende do Apple ID:

| Apple ID | Validade da instalação |
|---|---|
| Gratuito | 7 dias; depois reinstale (o AltStore renova sozinho) |
| Conta de desenvolvedor (US$ 99/ano) | 1 ano |

Com um Mac e o Xcode, `flutter run --release` instala direto no iPhone conectado.

## Compilar

Precisa de:
- Rust;
- Flutter 3.50+;
- para Android: Android SDK + NDK e JDK 21;
- para iPhone: um Mac com Xcode.

```sh
# Uma vez:
rustup target add aarch64-linux-android armv7-linux-androideabi x86_64-linux-android   # Android
rustup target add aarch64-apple-ios aarch64-apple-ios-sim                               # iPhone
cargo install flutter_rust_bridge_codegen --version 2.13.0 --locked

cd mobile
flutter pub get
flutter build apk --release          # Android: build/app/outputs/flutter-apk/app-release.apk
flutter build ios --release          # iPhone (no Mac)
flutter run                          # no celular conectado / emulador
```

Depois de mudar a API em `crates/mobile/src/api`:

```sh
cd mobile
flutter_rust_bridge_codegen generate
cargo fmt --all
```

O código gerado (`crates/mobile/src/frb_generated.rs` e `lib/src/rust/`) fica no repositório.

## A chave de assinatura do Android

Todo APK publicado precisa ser assinado com **a mesma chave**: o Android recusa atualizar o app com um APK assinado por outra. Crie a chave uma vez e guarde no 1Password:

```sh
keytool -genkeypair -v -keystore rdesk-release.jks -alias rdesk -keyalg EC -groupname secp256r1 -validity 36500
```

- **Para compilar no seu computador**: crie `mobile/android/key.properties` (já está no `.gitignore`):

  ```properties
  storeFile=C:/caminho/rdesk-release.jks
  storePassword=…
  keyAlias=rdesk
  keyPassword=…
  ```

  Sem esse arquivo, o build usa a chave de debug. Funciona para testar, mas não atualiza um APK publicado.
- **Para o CI** (workflow *Release*): crie os *Repository secrets*:

  | Secret | Conteúdo |
  |---|---|
  | `RDESK_ANDROID_KEYSTORE_B64` | o `.jks` em base64 (`base64 -w0 rdesk-release.jks`) |
  | `RDESK_ANDROID_KEYSTORE_PASSWORD` | a senha do keystore |
  | `RDESK_ANDROID_KEY_ALIAS` | o alias (`rdesk`) |
  | `RDESK_ANDROID_KEY_PASSWORD` | a senha da chave |

  Sem eles, o job do Android falha de propósito.

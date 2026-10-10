# Configurar credenciales del launcher

El launcher usa dos identificadores públicos distintos. No son contraseñas ni claves privadas; guardarlos en la configuración local del launcher no permite entrar a tu cuenta por sí solo.

## Inicio de sesión Microsoft

El launcher no muestra el Client ID en Ajustes ni lo guarda en preferencias. El propietario configura el ID público de la app de escritorio una sola vez como variable de GitHub Actions `ETERNALCRAFT_MICROSOFT_CLIENT_ID`. No es un secreto y no se debe crear un client secret. El workflow de release rechaza IDs que no tengan formato UUID y no publica una versión jugable sin esta variable.

Para registrar la app: abre [Microsoft Entra](https://entra.microsoft.com/), entra a **App registrations → New registration**, registra `EternalCraft Launcher` para cuentas Microsoft personales y agrega `http://localhost` en **Authentication → Mobile and desktop applications**. Copia **Application (client) ID** desde Overview y guárdalo como variable de Actions en `Santi-PdR/EternalCraft-Launcher`. El launcher usa un puerto local disponible y PKCE al iniciar sesión.

El inicio requiere una cuenta Microsoft con licencia de Minecraft; configurar el ID no concede una licencia. Si el ID no está en una compilación, el launcher permite explorar pero desactiva el inicio de sesión y el lanzamiento.

Guía oficial: [registrar una aplicación en Microsoft Entra](https://learn.microsoft.com/en-us/entra/identity-platform/quickstart-register-app) y [registrar una aplicación de escritorio](https://learn.microsoft.com/en-us/entra/identity-platform/quickstart-desktop-app-sign-in).

## Autorización de publicación GitHub

1. El propietario registra la GitHub App y activa **Enable Device Flow**, con únicamente **Contents: Read and write**, instalada solo en `Santi-PdR/EternalCraft-Launcher`.
2. El Client ID público se configura en el build oficial mediante la variable de Actions `ETERNALCRAFT_GITHUB_APP_CLIENT_ID`; no se solicita a los usuarios ni se guarda en sus preferencias.
3. En Developer, pulsa **Autorizar GitHub** y completa el código de dispositivo en github.com. La cuenta también debe tener permiso de escritura en el repositorio.

No generes ni compartas una clave privada para instalar el launcher. La autorización exige tanto el permiso limitado de la App como que la cuenta conectada tenga acceso de escritura al repositorio. Revoca el acceso desde GitHub si un developer deja de colaborar.

Guía oficial: [registrar una GitHub App](https://docs.github.com/en/apps/creating-github-apps/registering-a-github-app/registering-a-github-app) y [elegir permisos mínimos](https://docs.github.com/en/apps/creating-github-apps/registering-a-github-app/choosing-permissions-for-a-github-app).

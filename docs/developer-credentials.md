# Configurar credenciales del launcher

El launcher usa dos identificadores públicos distintos. No son contraseñas ni claves privadas; guardarlos en la configuración local del launcher no permite entrar a tu cuenta por sí solo.

## Inicio de sesión Microsoft

1. Abre el [portal Microsoft Entra](https://entra.microsoft.com/) y entra a **App registrations → New registration**.
2. Registra `EternalCraft Launcher` y elige **Personal Microsoft accounts only** para usuarios de Xbox/Minecraft. Si también necesitas cuentas laborales, elige la opción que incluye cuentas organizacionales y personales.
3. En **Authentication → Add a platform**, elige **Mobile and desktop applications** y agrega el URI `http://localhost`. El launcher abre el navegador del sistema y usa un puerto local disponible con PKCE.
4. En la página **Overview**, copia **Application (client) ID**. Pégalo en Developer → Inicio de sesión Microsoft y pulsa **Guardar configuración Microsoft**.

No crees un client secret para el launcher. El inicio requiere una cuenta Microsoft con licencia de Minecraft; configurar el ID no concede una licencia.

Guía oficial: [registrar una aplicación en Microsoft Entra](https://learn.microsoft.com/en-us/entra/identity-platform/quickstart-register-app) y [registrar una aplicación de escritorio](https://learn.microsoft.com/en-us/entra/identity-platform/quickstart-desktop-app-sign-in).

## Autorización de publicación GitHub

1. Abre [New GitHub App](https://github.com/settings/apps/new) desde la cuenta que administra el repositorio.
2. Ponle un nombre identificable. Activa **Enable Device Flow**. No hace falta configurar callback URL para este flujo.
3. En **Repository permissions**, establece únicamente **Contents: Read and write**. Esa autorización permite publicar releases y escribir los manifiestos del repositorio.
4. Guarda la App e instálala en `Santi-PdR/EternalCraft-Launcher` seleccionando solo ese repositorio.
5. Copia su **Client ID** público en Developer → Configuración de acceso de publicación y pulsa **Guardar ID de GitHub**. Después pulsa **Autorizar GitHub** y completa el código de dispositivo en github.com.

No generes ni compartas una clave privada para instalar el launcher. La autorización exige tanto el permiso limitado de la App como que la cuenta conectada tenga acceso de escritura al repositorio. Revoca el acceso desde GitHub si un developer deja de colaborar.

Guía oficial: [registrar una GitHub App](https://docs.github.com/en/apps/creating-github-apps/registering-a-github-app/registering-a-github-app) y [elegir permisos mínimos](https://docs.github.com/en/apps/creating-github-apps/registering-a-github-app/choosing-permissions-for-a-github-app).

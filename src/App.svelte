<script lang="ts">
  import { convertFileSrc, invoke } from '@tauri-apps/api/core';
  import { listen } from '@tauri-apps/api/event';
  import { defaultWindowIcon, getVersion } from '@tauri-apps/api/app';
  import { getCurrentWindow } from '@tauri-apps/api/window';
  import { check } from '@tauri-apps/plugin-updater';
  import { relaunch } from '@tauri-apps/plugin-process';
  import { onMount } from 'svelte';
  import type { Bootstrap, DeveloperLoginStatus, InstallProgress, MinecraftStatus, ModInventory, PackPublishProgress, PackSourcePreview, PackSyncProgress, PackSyncResult, Series } from './lib/types';

  let bootstrap = $state<Bootstrap | null>(null);
  let selected = $derived.by(() => {
    const current = bootstrap;
    return current?.series.find((series) => series.id === current.activeSeriesId);
  });
  let activePage = $state<'home' | 'mods' | 'developer' | 'settings' | 'support'>('home');
  let settingsTab = $state<'game' | 'account' | 'appearance' | 'storage'>('game');
  let welcomeOpen = $state(false);
  let firstRunStep = $state<'account' | 'install'>('account');
  let loading = $state(true);
  let busy = $state(false);
  let error = $state('');
  let notice = $state('');
  let javaRefreshing = $state(false);
  let memoryDraft = $state(0);
  let minecraftStatus = $state<MinecraftStatus>({ running: false, seriesId: null, pid: null, exitCode: null, exitSuccess: null });
  let modInventory = $state<ModInventory | null>(null);
  let modsLoading = $state(false);
  let modsBusy = $state(false);
  let modsError = $state('');
  let installingSeries = $state<string | null>(null);
  let syncingPack = $state(false);
  let packSyncMessage = $state('');
  let installMessage = $state('');
  let launcherLogs = $state('Todavía no hay registros de instalación.');
  let logsLoading = $state(false);
  let launcherVersion = $state('');
  let launcherUpdateState = $state<'idle' | 'checking' | 'unsupported' | 'current' | 'available' | 'installing' | 'error'>('idle');
  let launcherUpdateVersion = $state('');
  let launcherUpdateMessage = $state('');
  let launcherUpdateNotes = $state('');
  let launcherUpdateDownloaded = $state(0);
  let launcherUpdateSize = $state<number | null>(null);
  let developerLoginStatus = $state<DeveloperLoginStatus>({ status: 'signedOut', username: null, userCode: null, verificationUri: null, expiresInSeconds: null, intervalSeconds: null, message: null });
  let developerLoginBusy = $state(false);
  let developerPollBusy = false;
  let packSourcePreview = $state<PackSourcePreview | null>(null);
  let packSourceBusy = $state(false);
  let licenseReviewFingerprint = $state('');
  let permissionRequiredCount = $derived(packSourcePreview?.files.filter((file) => file.licenseStatus === 'permissionRequired').length ?? 0);
  let unknownLicenseCount = $derived(packSourcePreview?.files.filter((file) => file.licenseStatus === 'unknown').length ?? 0);
  let incompatibleMinecraftCount = $derived(packSourcePreview?.files.filter((file) => file.minecraftCompatibility === 'incompatible').length ?? 0);
  let unknownMinecraftCount = $derived(packSourcePreview?.files.filter((file) => file.minecraftCompatibility === 'unknown').length ?? 0);
  let publishingPack = $state(false);
  let packPublishMessage = $state('');
  let publishVersion = $state('1.0.0');
  let backgroundUrl = $derived(bootstrap?.backgroundPath ? convertFileSrc(bootstrap.backgroundPath) : '');
  let themeAccent = $derived(bootstrap?.themeId === 'ghouls' ? '#bf624d' : bootstrap?.themeId === 'siege' ? '#d1a35b' : selected?.accent ?? '#d1a35b');

  onMount(() => {
    let disposed = false;
    const unlisteners: Array<() => void> = [];
    void defaultWindowIcon().then((icon) => icon ? getCurrentWindow().setIcon(icon) : undefined).catch(() => undefined);
    const refreshMinecraftStatus = async () => {
      try {
        minecraftStatus = await invoke<MinecraftStatus>('get_minecraft_status');
      } catch (reason) {
        if (!disposed) error = `No se pudo consultar el estado de Minecraft: ${String(reason)}`;
      }
    };
    const subscribe = <T,>(event: string, callback: (payload: T) => void) => {
      void listen<T>(event, ({ payload }) => callback(payload)).then((stop) => {
        if (disposed) stop();
        else unlisteners.push(stop);
      });
    };
    subscribe<InstallProgress>('forge-install-progress', (payload) => {
      if (payload.seriesId === installingSeries) installMessage = payload.message;
    });
    subscribe<PackSyncProgress>('pack-sync-progress', (payload) => {
      if (payload.seriesId === selected?.id) packSyncMessage = payload.message;
    });
    subscribe<PackPublishProgress>('pack-publish-progress', (payload) => {
      if (payload.seriesId === selected?.id) packPublishMessage = `${payload.completedFiles}/${payload.totalFiles} · ${payload.message}`;
    });
    void refreshMinecraftStatus();
    void getVersion().then((version) => { if (!disposed) launcherVersion = version; });
    const updateTimer = window.setTimeout(() => { if (!disposed) void checkLauncherUpdate(); }, 1800);
    const statusTimer = window.setInterval(() => { if (document.visibilityState === 'visible') void refreshMinecraftStatus(); }, 10000);
    const developerTimer = window.setInterval(() => {
      if (developerLoginStatus.status === 'pending' && !developerPollBusy) void pollGitHubDeveloperLogin();
    }, Math.max(5000, (developerLoginStatus.intervalSeconds ?? 5) * 1000));
    return () => {
      disposed = true;
      window.clearTimeout(updateTimer);
      window.clearInterval(statusTimer);
      window.clearInterval(developerTimer);
      unlisteners.forEach((stop) => stop());
    };
  });

  async function loadMods(seriesId: string | undefined) {
    if (!seriesId) return;
    modsLoading = true;
    modsError = '';
    modInventory = null;
    try {
      modInventory = await invoke<ModInventory>('get_mod_inventory', { seriesId });
    } catch (reason) {
      modsError = String(reason);
    } finally {
      modsLoading = false;
    }
  }

  async function addPersonalMod() {
    if (!selected) return;
    modsBusy = true;
    modsError = '';
    try {
      modInventory = await invoke<ModInventory>('add_personal_mod_to_series', { seriesId: selected.id });
    } catch (reason) {
      modsError = String(reason);
    } finally {
      modsBusy = false;
    }
  }

  async function removePersonalMod(fileName: string) {
    if (!selected || !window.confirm(`¿Quitar ${fileName} de los mods personales de esta instancia?`)) return;
    modsBusy = true;
    modsError = '';
    try {
      modInventory = await invoke<ModInventory>('remove_personal_mod_from_series', { seriesId: selected.id, fileName });
    } catch (reason) {
      modsError = String(reason);
    } finally {
      modsBusy = false;
    }
  }

  async function activatePersonalMod(fileName: string) {
    if (!selected) return;
    modsBusy = true;
    modsError = '';
    try {
      modInventory = await invoke<ModInventory>('activate_personal_mod_for_series', { seriesId: selected.id, fileName });
    } catch (reason) {
      modsError = String(reason);
    } finally {
      modsBusy = false;
    }
  }

  function personalModLoaded(fileName: string) {
    return Boolean(modInventory?.loadedFromModsRoot.some((file) => file.name.toLowerCase() === fileName.toLowerCase()));
  }

  async function checkLauncherUpdate() {
    if (launcherUpdateState === 'checking' || launcherUpdateState === 'installing') return;
    launcherUpdateState = 'checking';
    launcherUpdateMessage = 'Consultando el canal oficial y verificando la firma…';
    launcherUpdateNotes = '';
    launcherUpdateVersion = '';
    launcherUpdateDownloaded = 0;
    try {
      const supported = await invoke<boolean>('launcher_updater_supported');
      if (!supported) {
        launcherUpdateState = 'unsupported';
        launcherUpdateMessage = 'La actualización integrada requiere AppImage en Linux. Este paquete se actualiza instalando el RPM/DEB más reciente.';
        return;
      }
    } catch (reason) {
      launcherUpdateState = 'error';
      launcherUpdateMessage = `No se pudo detectar el tipo de instalación: ${String(reason)}`;
      return;
    }
    launcherUpdateSize = null;
    try {
      const update = await check();
      if (!update) {
        launcherUpdateState = 'current';
        launcherUpdateMessage = 'Ya tienes la versión más reciente.';
        return;
      }
      launcherUpdateVersion = update.version;
      launcherUpdateNotes = update.body ?? '';
      launcherUpdateState = 'available';
      launcherUpdateMessage = `Nueva versión ${update.version} disponible. La firma se comprobará antes de instalar.`;
    } catch (reason) {
      launcherUpdateState = 'error';
      launcherUpdateMessage = `No se pudo comprobar la actualización: ${String(reason)}`;
    }
  }

  async function installLauncherUpdate() {
    if (launcherUpdateState !== 'available') return;
    launcherUpdateState = 'installing';
    launcherUpdateMessage = 'Preparando la actualización firmada…';
    launcherUpdateDownloaded = 0;
    launcherUpdateSize = null;
    try {
      const update = await check();
      if (!update) {
        launcherUpdateState = 'current';
        launcherUpdateMessage = 'La actualización ya no está disponible; tienes la versión más reciente.';
        return;
      }
      if (update.version !== launcherUpdateVersion) {
        launcherUpdateVersion = update.version;
        launcherUpdateNotes = update.body ?? '';
      }
      await update.downloadAndInstall((event) => {
        if (event.event === 'Started') {
          launcherUpdateSize = event.data.contentLength ?? null;
          launcherUpdateMessage = 'Descargando el paquete de actualización…';
        } else if (event.event === 'Progress') {
          launcherUpdateDownloaded += event.data.chunkLength;
        } else if (event.event === 'Finished') {
          launcherUpdateMessage = 'Descarga y firma verificadas. Reiniciando EternalCraft…';
        }
      });
      await relaunch();
    } catch (reason) {
      launcherUpdateState = 'error';
      launcherUpdateMessage = `No se pudo instalar la actualización: ${String(reason)}`;
    }
  }

  async function refresh() {
    loading = true;
    error = '';
    try {
      bootstrap = await invoke<Bootstrap>('get_bootstrap');
      if (!localStorage.getItem('eternalcraft-welcome-v1')) {
        firstRunStep = bootstrap.microsoftProfile ? 'install' : 'account';
        welcomeOpen = true;
      }
      memoryDraft = bootstrap.memory.selectedMb;
      developerLoginStatus = await invoke<DeveloperLoginStatus>('github_developer_status');
      await loadMods(bootstrap.activeSeriesId);
    } catch (reason) {
      error = String(reason);
    } finally {
      loading = false;
    }
  }

  async function beginGitHubDeveloperLogin() {
    developerLoginBusy = true;
    error = '';
    try {
      developerLoginStatus = await invoke<DeveloperLoginStatus>('begin_github_developer_login');
      notice = developerLoginStatus.message ?? 'Autoriza el launcher en GitHub para continuar';
    } catch (reason) {
      error = String(reason);
    } finally {
      developerLoginBusy = false;
    }
  }

  async function pollGitHubDeveloperLogin() {
    if (developerPollBusy) return;
    developerPollBusy = true;
    try {
      developerLoginStatus = await invoke<DeveloperLoginStatus>('poll_github_developer_login');
      if (developerLoginStatus.status === 'authorized') {
        bootstrap = await invoke<Bootstrap>('get_bootstrap');
        notice = `Developer conectado como ${developerLoginStatus.username}`;
      } else if (developerLoginStatus.status !== 'pending' && developerLoginStatus.message) {
        notice = developerLoginStatus.message;
      }
    } catch (reason) {
      error = String(reason);
    } finally {
      developerPollBusy = false;
    }
  }

  async function logoutGitHubDeveloper() {
    developerLoginBusy = true;
    error = '';
    try {
      developerLoginStatus = await invoke<DeveloperLoginStatus>('logout_github_developer');
      packSourcePreview = null;
      licenseReviewFingerprint = '';
      bootstrap = await invoke<Bootstrap>('get_bootstrap');
      notice = developerLoginStatus.message ?? 'Sesión developer cerrada';
    } catch (reason) {
      error = String(reason);
    } finally {
      developerLoginBusy = false;
    }
  }

  async function loginMicrosoft() {
    if (!bootstrap?.microsoftLoginAvailable) {
      error = 'El inicio de sesión Microsoft no está habilitado en esta versión. Puedes explorar el launcher; para jugar hace falta una cuenta Microsoft con Minecraft.';
      return;
    }
    busy = true;
    error = '';
    notice = 'Completa el inicio de sesión en el navegador que se abrió…';
    try {
      bootstrap = await invoke<Bootstrap>('login_microsoft');
      notice = `Sesión iniciada como ${bootstrap.microsoftProfile?.username ?? 'Minecraft'}`;
      if (welcomeOpen) firstRunStep = 'install';
    } catch (reason) {
      error = String(reason);
      notice = '';
    } finally {
      busy = false;
    }
  }

  async function logoutMicrosoft() {
    busy = true;
    error = '';
    try {
      bootstrap = await invoke<Bootstrap>('logout_microsoft');
      notice = 'Se cerró la sesión; las credenciales solo estaban en la memoria de esta ejecución';
    } catch (reason) {
      error = String(reason);
    } finally {
      busy = false;
    }
  }

  async function launchMinecraft() {
    if (!selected) return;
    busy = true;
    error = '';
    try {
      minecraftStatus = await invoke<MinecraftStatus>('launch_minecraft', { seriesId: selected.id });
      notice = `Minecraft se inició${minecraftStatus.pid ? ` (PID ${minecraftStatus.pid})` : ''}. El launcher seguirá disponible para ver su estado y registros.`;
    } catch (reason) {
      error = String(reason);
    } finally {
      busy = false;
    }
  }

  async function chooseSeries(id: string) {
    if (!bootstrap || id === bootstrap.activeSeriesId) return;
    busy = true;
    error = '';
    try {
      bootstrap = await invoke<Bootstrap>('set_active_series', { seriesId: id });
      await loadMods(id);
      notice = 'Serie activa guardada';
    } catch (reason) {
      error = String(reason);
    } finally {
      busy = false;
    }
  }

  async function selectDirectory() {
    if (!selected) return;
    busy = true;
    error = '';
    try {
      bootstrap = await invoke<Bootstrap>('select_game_directory', { seriesId: selected.id });
      await loadMods(selected.id);
      notice = 'Carpeta de juego guardada';
    } catch (reason) {
      error = String(reason);
    } finally {
      busy = false;
    }
  }

  async function linkDetectedDirectory() {
    if (!selected) return;
    busy = true;
    error = '';
    try {
      bootstrap = await invoke<Bootstrap>('link_detected_directory', { seriesId: selected.id });
      await loadMods(selected.id);
      notice = 'Instancia detectada vinculada al launcher';
    } catch (reason) {
      error = String(reason);
    } finally {
      busy = false;
    }
  }

  async function saveMemoryLimit() {
    if (!bootstrap) return;
    busy = true;
    error = '';
    try {
      bootstrap = await invoke<Bootstrap>('set_memory_limit', { memoryMb: memoryDraft });
      memoryDraft = bootstrap.memory.selectedMb;
      notice = `Memoria JVM guardada: ${bootstrap.memory.selectedMb} MiB`;
    } catch (reason) {
      error = String(reason);
    } finally {
      busy = false;
    }
  }

  async function resetMemoryLimit() {
    busy = true;
    error = '';
    try {
      bootstrap = await invoke<Bootstrap>('reset_memory_limit');
      memoryDraft = bootstrap.memory.selectedMb;
      notice = 'Memoria JVM ajustada automáticamente';
    } catch (reason) {
      error = String(reason);
    } finally {
      busy = false;
    }
  }

  async function refreshJava() {
    javaRefreshing = true;
    error = '';
    try {
      bootstrap = await invoke<Bootstrap>('refresh_java_status');
      notice = bootstrap.java.detail;
    } catch (reason) {
      error = String(reason);
    } finally {
      javaRefreshing = false;
    }
  }

  async function selectJava() {
    busy = true;
    error = '';
    try {
      bootstrap = await invoke<Bootstrap>('select_java_executable');
      notice = 'Java 17 seleccionado y guardado';
    } catch (reason) {
      error = String(reason);
    } finally {
      busy = false;
    }
  }

  async function resetJava() {
    busy = true;
    error = '';
    try {
      bootstrap = await invoke<Bootstrap>('reset_java_selection');
      notice = 'Se restauró la detección automática de Java';
    } catch (reason) {
      error = String(reason);
    } finally {
      busy = false;
    }
  }

  async function saveTheme(themeId: string) {
    try { bootstrap = await invoke<Bootstrap>('set_theme', { themeId }); notice = 'Tema guardado'; }
    catch (reason) { error = String(reason); }
  }

  async function chooseBackground() {
    try { bootstrap = await invoke<Bootstrap>('select_background'); notice = bootstrap.backgroundPath ? 'Fondo guardado en este equipo' : ''; }
    catch (reason) { error = String(reason); }
  }

  async function clearBackground() {
    try { bootstrap = await invoke<Bootstrap>('clear_background'); notice = 'Se restauró el fondo predeterminado'; }
    catch (reason) { error = String(reason); }
  }

  async function installBase() {
    if (!selected) return;
    const seriesId = selected.id;
    installingSeries = seriesId;
    installMessage = 'Iniciando la instalación verificada de Forge…';
    error = '';
    try {
      bootstrap = await invoke<Bootstrap>('install_forge_base', { seriesId });
      await loadMods(seriesId);
      notice = 'La base de Minecraft y Forge quedó instalada y verificada. El pack todavía no está publicado.';
    } catch (reason) {
      error = String(reason);
    } finally {
      installingSeries = null;
    }
  }

  async function syncOfficialPack() {
    if (!selected || selected.packStatus !== 'available') return;
    syncingPack = true;
    packSyncMessage = 'Consultando la versión oficial…';
    error = '';
    try {
      const result = await invoke<PackSyncResult>('sync_official_pack', { seriesId: selected.id });
      await loadMods(selected.id);
      notice = `Pack ${result.version} sincronizado: ${result.downloadedFiles} mods oficiales; ${result.removedFiles} retirados.`;
    } catch (reason) {
      error = String(reason);
    } finally {
      syncingPack = false;
      packSyncMessage = '';
    }
  }

  function pathFor(series: Series) {
    return bootstrap?.gameDirectories[series.id] || bootstrap?.suggestedDirectories[series.id] || '';
  }

  function isLinked(series: Series) {
    return Boolean(bootstrap?.gameDirectories[series.id]);
  }

  function isDetected(series: Series) {
    return !isLinked(series) && Boolean(bootstrap?.suggestedDirectories[series.id]);
  }

  function openMods() {
    activePage = 'mods';
    void loadMods(bootstrap?.activeSeriesId);
  }

  async function choosePackSource() {
    if (!selected || !developerLoginStatus.username) return;
    licenseReviewFingerprint = '';
    packSourceBusy = true;
    error = '';
    try {
      packSourcePreview = await invoke<PackSourcePreview | null>('choose_pack_source_directory', { seriesId: selected.id });
      if (packSourcePreview) notice = `${packSourcePreview.files.length} mods oficiales verificados para ${selected.name}`;
    } catch (reason) {
      error = String(reason);
    } finally {
      packSourceBusy = false;
    }
  }

  async function refreshPackSource() {
    if (!selected || !developerLoginStatus.username) return;
    licenseReviewFingerprint = '';
    packSourceBusy = true;
    error = '';
    try {
      packSourcePreview = await invoke<PackSourcePreview>('refresh_pack_source_preview', { seriesId: selected.id });
      notice = `Fuente revisada: ${packSourcePreview.files.length} mods`;
    } catch (reason) {
      error = String(reason);
    } finally {
      packSourceBusy = false;
    }
  }

  async function publishPack() {
    if (!selected || !developerLoginStatus.username || packSourcePreview?.seriesId !== selected.id) return;
    if (licenseReviewFingerprint !== packSourcePreview.sourceFingerprint) {
      error = 'Revisa las licencias y confirma que tienes permiso para redistribuir todos los JAR incluidos.';
      return;
    }
    if (!window.confirm(`Publicar ${selected.name} ${publishVersion} en GitHub? Esta versión quedará disponible para todos los usuarios del launcher.`)) return;
    publishingPack = true;
    packPublishMessage = 'Preparando release borrador…';
    error = '';
    try {
      notice = await invoke<string>('publish_pack_release', { seriesId: selected.id, version: publishVersion.trim(), confirmedSourceFingerprint: licenseReviewFingerprint });
      bootstrap = await invoke<Bootstrap>('get_bootstrap');
      packSourcePreview = null;
      licenseReviewFingerprint = '';
    } catch (reason) {
      error = String(reason);
    } finally {
      publishingPack = false;
      packPublishMessage = '';
    }
  }

  async function openSupport() {
    activePage = 'support';
    logsLoading = true;
    error = '';
    try {
      const seriesId = bootstrap?.activeSeriesId;
      launcherLogs = await invoke<string>('get_launcher_logs', { seriesId });
    } catch (reason) {
      error = String(reason);
    } finally {
      logsLoading = false;
    }
  }

  function continueWithoutAccount() {
    localStorage.setItem('eternalcraft-welcome-v1', 'done');
    firstRunStep = 'install';
  }

  function finishWelcome() {
    localStorage.setItem('eternalcraft-welcome-v1', 'done');
    welcomeOpen = false;
  }

  async function copySupportDiagnostics() {
    if (!bootstrap || !selected) return;
    try {
      await navigator.clipboard.writeText(`EternalCraft ${launcherVersion} · ${selected.name} · Minecraft ${selected.minecraftVersion} · ${selected.loader} ${selected.loaderVersion} · Java ${bootstrap.java.version ?? 'no detectado'} · ${minecraftStatus.running ? 'Minecraft en ejecución' : 'Minecraft cerrado'}\nRegistro local: ${bootstrap.logFile}`);
      notice = 'Resumen de diagnóstico copiado';
    } catch {
      error = 'No se pudo copiar el diagnóstico al portapapeles.';
    }
  }

  function formatBytes(bytes: number) {
    if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(0)} KB`;
    return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
  }

  refresh();
</script>

<svelte:head>
  <title>{selected?.name ? `${selected.name} · EternalCraft` : 'EternalCraft Launcher'}</title>
  <meta name="description" content="Launcher oficial de las series EternalCraft" />
</svelte:head>

<div class="shell" data-theme={bootstrap?.themeId ?? 'series'} style:--accent={themeAccent}>
  <aside class="sidebar">
    <a class="brand" href="#inicio" onclick={() => (activePage = 'home')} aria-label="EternalCraft inicio">
      <span class="brand-mark">EC</span>
      <span><strong>ETERNALCRAFT</strong><small>GAME HUB</small></span>
    </a>

    {#if bootstrap}
      <div class="series-block">
        <span class="eyebrow">SERIE ACTIVA</span>
        <div class="series-list">
          {#each bootstrap.series as series (series.id)}
            <button class:active={series.id === bootstrap?.activeSeriesId} class="series-choice" onclick={() => chooseSeries(series.id)} disabled={busy || installingSeries !== null || modsBusy || syncingPack}>
              <span class="series-dot" style:--series-accent={series.accent}></span>
              <span>{series.name}</span>
              {#if series.id === bootstrap?.activeSeriesId}<span class="check">✓</span>{/if}
            </button>
          {/each}
        </div>
      </div>
    {/if}

    <nav aria-label="Navegación principal">
      <span class="eyebrow nav-caption">LAUNCHER</span>
      <button class:current={activePage === 'home'} onclick={() => (activePage = 'home')}><span>⌂</span> Inicio</button>
      <button class:current={activePage === 'mods'} onclick={openMods}><span>▦</span> Mods</button>
      <button class:current={activePage === 'developer'} onclick={() => (activePage = 'developer')}><span>⬆</span> Developer</button>
      <span class="eyebrow nav-caption">PREFERENCIAS</span>
      <button class:current={activePage === 'settings'} onclick={() => (activePage = 'settings')}><span>⚙</span> Ajustes</button>
      <button class:current={activePage === 'support'} onclick={openSupport}><span>?</span> Soporte</button>
    </nav>

    <div class="sidebar-footer"><span class="status-light"></span> ETERNALCRAFT <small>LAUNCHER NATIVO · v{launcherVersion || '0.1.2'}</small></div>
  </aside>

  <main>
    <header class="topbar">
      <div class="breadcrumbs">ETERNALCRAFT <span>/</span> {activePage === 'home' ? 'INICIO' : activePage === 'mods' ? 'BIBLIOTECA' : activePage === 'developer' ? 'DEVELOPER' : activePage === 'support' ? 'SOPORTE' : 'AJUSTES'}</div>
      <div class="topbar-right"><span class="online-indicator"></span> INSTALACIÓN LOCAL</div>
    </header>

    {#if loading}
      <div class="center-state"><span class="spinner"></span><p>Preparando tu espacio de juego…</p></div>
    {:else if error && !bootstrap}
      <div class="center-state error-state"><span class="state-icon">!</span><h1>No se pudo iniciar</h1><p>{error}</p><button class="button primary" onclick={refresh}>Reintentar</button></div>
    {:else if bootstrap && selected}
      {#if error}<div class="toast error-toast" role="alert">{error}</div>{/if}
      {#if notice}<div class="toast" role="status">{notice}</div>{/if}
      {#if launcherUpdateState === 'available'}<div class="toast launcher-update-toast" role="status"><span>EternalCraft {launcherUpdateVersion} está listo</span><button class="button primary" onclick={installLauncherUpdate}>Actualizar</button></div>{/if}

      {#if welcomeOpen}<div class="welcome-backdrop" role="presentation"><div class="welcome-dialog" role="dialog" aria-modal="true" aria-labelledby="welcome-title" tabindex="-1"><span class="eyebrow">BIENVENIDO A ETERNALCRAFT</span>{#if firstRunStep === 'account'}<h1 id="welcome-title">¿Cómo quieres empezar?</h1><p>Inicia sesión con Microsoft para jugar, o explora las series y ajustes antes de vincular una cuenta.</p><button class="button primary welcome-action" onclick={loginMicrosoft} disabled={busy || !bootstrap.microsoftLoginAvailable}>{busy ? 'Abriendo Microsoft…' : 'Iniciar sesión con Microsoft'}</button>{#if !bootstrap.microsoftLoginAvailable}<small>El inicio de sesión todavía no está habilitado en esta versión.</small>{/if}<button class="button secondary welcome-action" onclick={continueWithoutAccount}>Explorar sin iniciar sesión</button><small>Sin cuenta podrás recorrer el launcher. Para iniciar Minecraft se requiere una cuenta Microsoft con licencia.</small>{:else}<h1 id="welcome-title">Prepara tu primera serie</h1><p>{bootstrap.installedProfiles[selected.id] ? 'Tu instancia ya está lista.' : `Puedes instalar ahora Minecraft ${selected.minecraftVersion}, Forge y Java 17 para ${selected.name}.`}</p>{#if !bootstrap.installedProfiles[selected.id]}<button class="button primary welcome-action" onclick={() => { finishWelcome(); void installBase(); }}>Instalar Forge y Java 17</button>{/if}<button class="button secondary welcome-action" onclick={finishWelcome}>Lo haré después</button>{/if}</div></div>{/if}

      {#if activePage === 'home'}
        <section class="page home-page">
          <div class="page-heading"><div><span class="eyebrow">TU PRÓXIMA AVENTURA</span><h1>Elige tu mundo.</h1><p>Una biblioteca, distintas historias de EternalCraft.</p></div><span class="connection-pill"><i></i> Catálogo {bootstrap.catalogOnline ? 'actualizado' : 'sin conexión'}</span></div>

          <div class="hero-card" style:--hero-accent={themeAccent} style:--hero-background={backgroundUrl ? `url("${backgroundUrl}")` : 'none'}>
            <div class="hero-noise"></div><div class="hero-content">
              <span class="eyebrow">ETERNALCRAFT ORIGINAL SERIES</span>
              <h2>{selected.name}</h2><p class="hero-subtitle">{selected.subtitle}</p>
              <p class="hero-description">{selected.description}</p>
              <div class="hero-meta"><div><small>MINECRAFT</small><strong>{selected.minecraftVersion}</strong></div><div><small>CARGADOR</small><strong>{selected.loader} {selected.loaderVersion}</strong></div><div><small>ESTADO DEL PACK</small><strong class="muted-status">{selected.packStatus === 'available' ? 'Disponible' : 'Aún sin publicar'}</strong></div></div>
            </div>
            <div class="hero-side"><div class="series-emblem">{selected.id === 'siege' ? 'S' : 'G'}</div><span class="side-label">SERIE<br/>SELECCIONADA</span></div>
          </div>

          <div class="section-heading"><div><span class="eyebrow">UNIVERSOS</span><h2>Explora las series</h2></div><span class="quiet-count">{bootstrap.series.length} SERIES</span></div>
          <div class="series-grid">
            {#each bootstrap.series as series (series.id)}
              <button class:selected-card={series.id === bootstrap.activeSeriesId} class="series-card" style:--card-accent={series.accent} onclick={() => chooseSeries(series.id)} disabled={busy || installingSeries !== null || modsBusy || syncingPack}>
                <span class="card-orbit"></span><span class="card-kicker">ETERNALCRAFT</span><strong>{series.name}</strong><span class="card-subtitle">{series.subtitle}</span><span class="card-bottom">{series.minecraftVersion} <b>·</b> {series.loader} <span>↗</span></span>
              </button>
            {/each}
          </div>

          <div class="play-panel"><div class="play-copy"><span class="eyebrow">CARPETA DE JUEGO · {selected.name}</span><h3>{isLinked(selected) ? 'Carpeta seleccionada' : bootstrap.installedProfiles[selected.id] ? 'Instancia de EternalCraft' : 'Elige dónde instalar'}</h3><p>{bootstrap.installedProfiles[selected.id] && !isLinked(selected) ? bootstrap.managedGameDirectories[selected.id] : pathFor(selected) || 'Selecciona una carpeta de juego o instala una instancia administrada.'}</p>{#if minecraftStatus.running}<span class="saved-chip process-chip">MINECRAFT EN EJECUCIÓN · {minecraftStatus.seriesId === selected.id ? 'ESTA SERIE' : minecraftStatus.seriesId?.toUpperCase()} · PID {minecraftStatus.pid}</span>{:else if minecraftStatus.exitSuccess === false}<span class="warning-chip process-chip">ÚLTIMA SESIÓN CERRÓ CON ERROR · CÓDIGO {minecraftStatus.exitCode ?? 'DESCONOCIDO'}</span>{/if}</div><div class="play-actions"><button class="button secondary" onclick={selectDirectory} disabled={busy || syncingPack || minecraftStatus.running}>{isLinked(selected) ? 'Cambiar carpeta' : 'Seleccionar carpeta'}</button>{#if bootstrap.installedProfiles[selected.id]}<span class="saved-chip">FORGE INSTALADO</span>{:else}<button class="button secondary" onclick={installBase} disabled={installingSeries !== null || syncingPack || minecraftStatus.running} title="Instala una instancia aislada y descarga Java 17 de Mojang si hace falta">Instalar Forge + Java 17</button>{/if}{#if selected.packStatus === 'available'}<button class="button secondary" onclick={syncOfficialPack} disabled={syncingPack || minecraftStatus.running || !bootstrap.installedProfiles[selected.id] && !isLinked(selected)}>{syncingPack ? 'Actualizando mods…' : 'Instalar / actualizar pack'}</button>{#if bootstrap.microsoftProfile}<button class="button primary" onclick={launchMinecraft} disabled={busy || minecraftStatus.running || syncingPack || !bootstrap.installedProfiles[selected.id] && !isLinked(selected)}>{minecraftStatus.running ? 'Minecraft ejecutándose' : busy ? 'Preparando…' : 'Jugar'}</button>{/if}{:else}<button class="button primary" disabled title="El modpack de esta serie todavía no tiene archivos oficiales publicados">Pack no publicado</button>{/if}</div></div>
          {#if bootstrap.installedProfiles[selected.id]}<div class="managed-location"><span class="eyebrow">INSTANCIA ADMINISTRADA POR ETERNALCRAFT</span><code>{bootstrap.managedGameDirectories[selected.id]}</code></div>{/if}
          {#if installingSeries === selected.id}<div class="install-progress" role="status" aria-live="polite"><span class="spinner"></span><div><strong>{installMessage || 'Instalando Minecraft y Forge…'}</strong><p>Preparando el perfil del juego; Minecraft no se iniciará.</p></div></div>{/if}
          {#if syncingPack}<div class="install-progress" role="status" aria-live="polite"><span class="spinner"></span><div><strong>{packSyncMessage || 'Actualizando mods oficiales…'}</strong><p>El proceso verifica cada descarga antes de sustituir archivos administrados.</p></div></div>{/if}
          <p class="release-note">{selected.packStatus === 'available' ? 'El launcher verifica cada mod oficial y conserva tus archivos personales al sincronizar.' : 'Las versiones estarán disponibles cuando se publiquen manifiestos y archivos oficiales en este repositorio.'}</p>
        </section>
      {:else if activePage === 'mods'}
        <section class="page narrow-page"><div class="page-heading"><div><span class="eyebrow">CONTENIDO DEL JUEGO · {selected.name}</span><h1>Biblioteca de mods</h1><p>Los mods personales se guardan aparte y se enlazan a la raíz que Forge carga.</p></div><div class="play-actions"><button class="button secondary" onclick={addPersonalMod} disabled={modsBusy || modsLoading || !modInventory?.gameDirectory}>{modsBusy ? 'Guardando…' : 'Agregar mod personal'}</button><button class="button secondary" onclick={() => loadMods(selected.id)} disabled={modsLoading || modsBusy}>{modsLoading ? 'Leyendo…' : 'Actualizar inventario'}</button></div></div>
          {#if modsError}<div class="toast error-toast" role="alert">{modsError}</div>{/if}
          {#if !modInventory?.gameDirectory}<div class="empty-card"><div class="empty-icon">▦</div><h2>Vincula una carpeta de juego</h2><p>Elige una instancia en Inicio para revisar sus mods. La lectura del inventario es solo de consulta.</p><button class="button secondary" onclick={() => (activePage = 'home')}>Ir a Inicio</button></div>
          {:else if modsLoading}<div class="center-state"><span class="spinner"></span><p>Leyendo inventario de mods…</p></div>
          {:else if modInventory}<div class="settings-card inventory-card"><div class="inventory-path"><span class="eyebrow">CARPETA REVISADA</span><code>{modInventory.modsDirectory}</code></div><div class="inventory-stats"><article><strong>{modInventory.loadedFromModsRoot.length}</strong><span>JAR cargables en <code>mods/</code></span></article><article><strong>{modInventory.officialStore.length}</strong><span>Guardados en <code>mods/Oficiales/</code></span></article><article><strong>{modInventory.personalStore.length}</strong><span>Guardados en <code>mods/personales/</code></span></article></div><div class="inventory-warning"><b>Compatibilidad Forge:</b> Forge carga los JAR directamente desde <code>mods/</code>. Los archivos personales que agregues aquí se guardan en <code>mods/personales/</code> y el launcher los refleja en la raíz mediante enlace o copia.</div><div class="inventory-list"><h2>JAR que Forge encuentra en la raíz</h2>{#if modInventory.loadedFromModsRoot.length}<ul>{#each modInventory.loadedFromModsRoot as file (file.name)}<li><span>{file.name}</span><small>{formatBytes(file.sizeBytes)}</small></li>{/each}</ul>{:else}<p>No hay JAR directamente dentro de <code>mods/</code>.</p>{/if}</div><div class="inventory-list storage-list"><h2>Almacenamiento organizado</h2><div class="storage-columns"><div><h3>Oficiales</h3>{#if modInventory.officialStore.length}<ul>{#each modInventory.officialStore as file (file.name)}<li><span>{file.name}</span><small>{formatBytes(file.sizeBytes)}</small></li>{/each}</ul>{:else}<p>Sin JAR guardados aquí.</p>{/if}</div><div><h3>Personales</h3>{#if modInventory.personalStore.length}<ul>{#each modInventory.personalStore as file (file.name)}<li><span>{file.name}</span><small>{formatBytes(file.sizeBytes)}</small><button class="remove-mod" onclick={() => personalModLoaded(file.name) ? removePersonalMod(file.name) : activatePersonalMod(file.name)} disabled={modsBusy}>{personalModLoaded(file.name) ? 'Quitar' : 'Activar'}</button></li>{/each}</ul>{:else}<p>Sin JAR personales agregados desde este launcher.</p>{/if}</div></div></div></div>
          {/if}</section>
      {:else if activePage === 'support'}
        <section class="page narrow-page"><div class="page-heading"><div><span class="eyebrow">DIAGNÓSTICO LOCAL</span><h1>Soporte</h1><p>Estado de la serie activa y registros para localizar problemas.</p></div><div class="play-actions"><button class="button secondary" onclick={async () => { await openSupport(); }} disabled={logsLoading}>{logsLoading ? 'Leyendo…' : 'Actualizar diagnóstico'}</button><button class="button secondary" onclick={copySupportDiagnostics}>Copiar resumen</button></div></div><div class="diagnostic-grid"><article><span>CUENTA</span><strong>{bootstrap.microsoftProfile?.username ?? 'Sin sesión'}</strong></article><article><span>JAVA</span><strong>{bootstrap.java.compatible ? `Java ${bootstrap.java.version}` : 'Revisar Java 17'}</strong></article><article><span>INSTANCIA</span><strong>{bootstrap.installedProfiles[selected.id] ? 'Forge instalado' : pathFor(selected) ? 'Carpeta vinculada' : 'Sin configurar'}</strong></article><article><span>PACK</span><strong>{selected.packStatus === 'available' ? 'Disponible' : 'No publicado'}</strong></article></div><div class="settings-card log-card"><div class="inventory-path"><span class="eyebrow">ARCHIVO LOCAL · {selected.name}</span><code>{bootstrap.logFile}</code></div><pre class="log-viewer" aria-live="polite">{launcherLogs}</pre></div><p class="privacy-note">El resumen copiado omite credenciales. Los registros quedan en este equipo y solo se comparten si tú los envías.</p></section>
      {:else if activePage === 'developer'}
        <section class="page narrow-page"><div class="page-heading"><div><span class="eyebrow">PUBLICACIÓN DE CONTENIDO · {selected.name}</span><h1>Developer</h1><p>Prepara una fuente oficial de mods para la serie seleccionada. Solo se inspeccionan JAR en el nivel raíz; configs y carpetas personales quedan fuera.</p></div><span class="connection-pill">{developerLoginStatus.username ? `GITHUB · ${developerLoginStatus.username}` : 'GITHUB · DESCONECTADO'}</span></div>
          <details class="developer-integrations"><summary>Acceso Developer</summary>
            <p>GitHub comprueba tu cuenta y el permiso de escritura real sobre el repositorio. No se usa una contraseña compartida ni se guardan credenciales en la configuración del launcher.</p>
            {#if bootstrap.githubDeveloperEnabled}<p class="privacy-note">La autorización usa la GitHub App oficial. Solo las cuentas con permiso de escritura en el repositorio pueden publicar.</p>{:else}<p class="warning-chip">El acceso Developer no está configurado en esta compilación.</p>{/if}
            <div class="play-actions">{#if developerLoginStatus.username}<button class="button secondary" onclick={logoutGitHubDeveloper} disabled={developerLoginBusy}>Desconectar {developerLoginStatus.username}</button>{:else}<button class="button primary" onclick={beginGitHubDeveloperLogin} disabled={!bootstrap.githubDeveloperEnabled || developerLoginBusy || developerLoginStatus.status === 'pending'}>{developerLoginStatus.status === 'pending' ? 'Esperando autorización…' : 'Autorizar GitHub'}</button>{/if}</div>
            {#if developerLoginStatus.userCode}<div class="device-code" role="status"><strong>{developerLoginStatus.userCode}</strong><span>Ingresa este código en <a href={developerLoginStatus.verificationUri ?? 'https://github.com/login/device'} target="_blank" rel="noreferrer">GitHub Device Login ↗</a>. Caduca en {Math.ceil((developerLoginStatus.expiresInSeconds ?? 0) / 60)} min.</span></div>{/if}
            {#if developerLoginStatus.message}<p class="privacy-note">{developerLoginStatus.message}</p>{/if}
            <a class="text-link" href="https://github.com/Santi-PdR/EternalCraft-Launcher/blob/main/docs/developer-credentials.md" target="_blank" rel="noreferrer">Instrucciones completas ↗</a>
          </details>
          {#if !developerLoginStatus.username}<div class="empty-card developer-empty"><div class="empty-icon">⬆</div><h2>Acceso de publicación pendiente</h2><p>Configura la aplicación de GitHub y autoriza una cuenta con acceso al repositorio para publicar el pack de {selected.name}.</p></div>
          {:else}<div class="settings-card developer-pack-card"><div class="setting-row"><div><span class="eyebrow">FUENTE LOCAL · {selected.name}</span><h2>{packSourcePreview?.seriesId === selected.id ? `${packSourcePreview.files.length} JAR oficiales` : 'Selecciona la carpeta de mods'}</h2><p>{packSourcePreview?.seriesId === selected.id ? packSourcePreview.directory : 'Se leerán únicamente archivos .jar del directorio elegido. No se cargan configuraciones ni subcarpetas.'}</p></div><div class="play-actions"><button class="button secondary" onclick={choosePackSource} disabled={packSourceBusy}>{packSourceBusy ? 'Verificando…' : 'Elegir carpeta'}</button>{#if packSourcePreview?.seriesId === selected.id}<button class="button secondary" onclick={refreshPackSource} disabled={packSourceBusy}>Volver a verificar</button>{/if}</div></div>
            {#if packSourcePreview?.seriesId === selected.id}
              {@const sourceFingerprint = packSourcePreview.sourceFingerprint}
              <div class="inventory-stats"><article><strong>{packSourcePreview.files.length}</strong><span>Mods JAR verificados</span></article><article><strong>{formatBytes(packSourcePreview.totalBytes)}</strong><span>Tamaño total</span></article><article><strong>SHA-256</strong><span>Comprobado archivo por archivo</span></article></div>
              <div class="inventory-list"><h2>Archivos incluidos en la fuente</h2><ul>{#each packSourcePreview.files as file (file.name)}<li><span>{file.name}</span><small>{formatBytes(file.sizeBytes)} · SHA-256 {file.sha256.slice(0, 12)}… · Licencia: {file.license ?? 'no declarada'} · {file.licenseStatus === 'permissionRequired' ? 'requiere revisión/permiso' : file.licenseStatus === 'unknown' ? 'licencia por verificar' : 'revisar condiciones'} · Minecraft: {file.minecraftCompatibility === 'compatible' ? `compatible${file.minecraftVersionRange ? ` (${file.minecraftVersionRange})` : ''}` : file.minecraftCompatibility === 'incompatible' ? `incompatible${file.minecraftVersionRange ? ` (${file.minecraftVersionRange})` : ''}` : file.minecraftVersionRange ? `revisar manualmente (${file.minecraftVersionRange})` : 'rango no declarado'}</small></li>{/each}</ul></div>
              <div class="inventory-warning"><b>Revisión antes de publicar:</b> {permissionRequiredCount} JAR tienen condiciones restrictivas, {unknownLicenseCount} tienen licencia desconocida, {incompatibleMinecraftCount} declaran incompatibilidad con Minecraft {selected.minecraftVersion} y {unknownMinecraftCount} requieren revisar manualmente su metadato de compatibilidad. Los mods mantienen sus propias condiciones de licencia.
                <label class="license-confirm"><input type="checkbox" checked={licenseReviewFingerprint === sourceFingerprint} onchange={(event) => (licenseReviewFingerprint = event.currentTarget.checked ? sourceFingerprint : '')} /> Confirmo que revisé las licencias y tengo derecho a redistribuir públicamente todos los JAR de esta fuente.</label>
              </div>
              <div class="publish-controls"><label>Versión de esta serie<input class="text-input" value={publishVersion} oninput={(event) => (publishVersion = event.currentTarget.value)} placeholder="1.0.0" /></label><button class="button primary" onclick={publishPack} disabled={publishingPack || packSourceBusy || incompatibleMinecraftCount > 0 || licenseReviewFingerprint !== sourceFingerprint || !/^\d+\.\d+\.\d+$/.test(publishVersion.trim())}>{publishingPack ? 'Publicando…' : `Publicar ${selected.name}`}</button></div>
              {#if publishingPack}<div class="install-progress" role="status" aria-live="polite"><span class="spinner"></span><div><strong>{packPublishMessage}</strong><p>La publicación se puede reintentar si falla; el release conserva los assets verificados.</p></div></div>{/if}
            {/if}
            <p class="privacy-note">Se suben solo los mods JAR de la carpeta revisada. Los archivos de configuración, otras carpetas y datos personales no se incluyen.</p></div>{/if}</section>
      {:else}
        <section class="page narrow-page settings-page"><div class="page-heading"><div><span class="eyebrow">PREFERENCIAS LOCALES</span><h1>Ajustes</h1><p>Configura tu experiencia de EternalCraft.</p></div></div>
          <div class="settings-tabs" role="tablist" aria-label="Secciones de ajustes">
            <button class:tab-active={settingsTab === 'game'} onclick={() => (settingsTab = 'game')}>Juego</button>
            <button class:tab-active={settingsTab === 'account'} onclick={() => (settingsTab = 'account')}>Cuenta</button>
            <button class:tab-active={settingsTab === 'appearance'} onclick={() => (settingsTab = 'appearance')}>Apariencia</button>
            <button class:tab-active={settingsTab === 'storage'} onclick={() => (settingsTab = 'storage')}>Carpetas y soporte</button>
          </div>
          {#if settingsTab === 'game'}<div class="settings-card">
            <div class="setting-row memory-setting"><div><span class="eyebrow">MEMORIA DE MINECRAFT</span><h2>{(memoryDraft / 1024).toFixed(1)} GiB asignados</h2><p>RAM detectada: {(bootstrap.memory.totalMb / 1024).toFixed(1)} GiB · recomendado hasta {(bootstrap.memory.maxMb / 1024).toFixed(1)} GiB.</p><input class="memory-slider" type="range" min={bootstrap.memory.minMb} max={bootstrap.memory.maxMb} step="512" aria-label="Memoria RAM asignada a Minecraft" value={memoryDraft} oninput={(event) => (memoryDraft = Number(event.currentTarget.value))} /></div><div class="play-actions"><button class="button secondary" onclick={saveMemoryLimit} disabled={busy || memoryDraft === bootstrap.memory.selectedMb}>Guardar</button>{#if bootstrap.memory.manuallySelected}<button class="button secondary" onclick={resetMemoryLimit} disabled={busy}>Automático</button>{/if}</div></div>
            <div class="setting-row"><div><span class="eyebrow">JAVA · MINECRAFT 1.20.1</span><h2>{bootstrap.java.compatible ? `Java ${bootstrap.java.version}` : 'Java 17 no está listo'}</h2><p>{bootstrap.java.detail}{#if bootstrap.java.executable}<br/><code>{bootstrap.java.executable}</code>{/if}</p></div><div class="play-actions"><span class:saved-chip={bootstrap.java.compatible} class:warning-chip={!bootstrap.java.compatible}>{bootstrap.java.compatible ? 'LISTO' : 'REVISAR'}</span><button class="button secondary" onclick={selectJava} disabled={busy}>Elegir Java</button>{#if bootstrap.javaManuallySelected}<button class="button secondary" onclick={resetJava} disabled={busy}>Automático</button>{/if}<button class="button secondary" onclick={refreshJava} disabled={javaRefreshing}>{javaRefreshing ? 'Comprobando…' : 'Comprobar'}</button></div></div>
          </div>
          {:else if settingsTab === 'account'}<div class="settings-card"><div class="setting-row"><div><span class="eyebrow">CUENTA DE MINECRAFT</span><h2>{bootstrap.microsoftProfile?.username ?? 'No has iniciado sesión'}</h2><p>{bootstrap.microsoftProfile ? 'Cuenta Microsoft vinculada a esta sesión.' : 'Usa una cuenta Microsoft propietaria de Minecraft para jugar. Puedes recorrer el launcher sin iniciar sesión.'}</p></div><div class="play-actions">{#if bootstrap.microsoftProfile}<button class="button secondary" onclick={logoutMicrosoft} disabled={busy}>Cerrar sesión</button>{:else}<button class="button primary" onclick={loginMicrosoft} disabled={busy || !bootstrap.microsoftLoginAvailable}>{busy ? 'Abriendo…' : 'Iniciar sesión con Microsoft'}</button>{/if}</div></div>{#if !bootstrap.microsoftProfile && !bootstrap.microsoftLoginAvailable}<p class="account-setup-note">El inicio de Microsoft todavía no está habilitado en esta versión. Puedes recorrer el launcher; el modo de juego requiere una cuenta con licencia.</p>{/if}</div>
          {:else if settingsTab === 'appearance'}<div class="settings-card"><div class="setting-row appearance-row"><div><span class="eyebrow">TEMAS</span><h2>El aspecto de tu launcher</h2><p>Elige una paleta para la interfaz. Puedes agregar un fondo propio desde tu equipo.</p><div class="theme-options"><button class:theme-selected={bootstrap.themeId === 'light'} class="button secondary theme-swatch light-swatch" onclick={() => saveTheme('light')}>Claro</button><button class:theme-selected={bootstrap.themeId === 'series'} class="button secondary" onclick={() => saveTheme('series')}>Serie activa</button><button class:theme-selected={bootstrap.themeId === 'siege'} class="button secondary" onclick={() => saveTheme('siege')}>SIEGE</button><button class:theme-selected={bootstrap.themeId === 'ghouls'} class="button secondary" onclick={() => saveTheme('ghouls')}>Ghouls</button></div></div><div class="play-actions"><button class="button secondary" onclick={chooseBackground}>Elegir fondo</button>{#if bootstrap.backgroundPath}<button class="button secondary" onclick={clearBackground}>Quitar fondo</button>{/if}</div></div></div>
          {:else}<div class="settings-card"><div class="setting-row"><div><span class="eyebrow">INSTANCIA · {selected.name}</span><h2>Carpeta de juego</h2><p>{pathFor(selected) || bootstrap.managedGameDirectories[selected.id] || 'Elige una ubicación para esta serie.'}</p></div><div class="play-actions"><button class="button secondary" onclick={selectDirectory} disabled={busy}>{isLinked(selected) ? 'Cambiar carpeta' : 'Seleccionar carpeta'}</button><button class="button secondary" onclick={installBase} disabled={installingSeries !== null}>{bootstrap.installedProfiles[selected.id] ? 'Reparar base' : 'Instalar Forge + Java 17'}</button></div></div><div class="setting-row"><div><span class="eyebrow">PREFERENCIAS</span><h2>Guardadas en este equipo</h2><p>{bootstrap.configDirectory}</p></div><span class="saved-chip">GUARDADO</span></div><div class="setting-row"><div><span class="eyebrow">REGISTRO DEL LAUNCHER</span><h2>Diagnóstico y soporte</h2><p>Revisa estado del juego y registros locales sin compartirlos automáticamente.</p></div><button class="button secondary" onclick={openSupport}>Abrir soporte</button></div></div>{/if}
        </section>
      {/if}
    {/if}
  </main>
</div>

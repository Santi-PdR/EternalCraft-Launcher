<script lang="ts">
  import { convertFileSrc, invoke } from '@tauri-apps/api/core';
  import { listen } from '@tauri-apps/api/event';
  import { onMount } from 'svelte';
  import type { Bootstrap, InstallProgress, MinecraftStatus, ModInventory, PackSyncProgress, PackSyncResult, Series } from './lib/types';

  let bootstrap = $state<Bootstrap | null>(null);
  let selected = $derived.by(() => {
    const current = bootstrap;
    return current?.series.find((series) => series.id === current.activeSeriesId);
  });
  let activePage = $state<'home' | 'mods' | 'settings' | 'support'>('home');
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
  let clientIdDraft = $state('');
  let backgroundUrl = $derived(bootstrap?.backgroundPath ? convertFileSrc(bootstrap.backgroundPath) : '');
  let themeAccent = $derived(bootstrap?.themeId === 'ghouls' ? '#bf624d' : bootstrap?.themeId === 'siege' ? '#d1a35b' : selected?.accent ?? '#d1a35b');

  onMount(() => {
    let disposed = false;
    const unlisteners: Array<() => void> = [];
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
    void refreshMinecraftStatus();
    const statusTimer = window.setInterval(() => void refreshMinecraftStatus(), 2500);
    return () => {
      disposed = true;
      window.clearInterval(statusTimer);
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

  async function refresh() {
    loading = true;
    error = '';
    try {
      bootstrap = await invoke<Bootstrap>('get_bootstrap');
      clientIdDraft = bootstrap.microsoftClientId ?? '';
      memoryDraft = bootstrap.memory.selectedMb;
      await loadMods(bootstrap.activeSeriesId);
    } catch (reason) {
      error = String(reason);
    } finally {
      loading = false;
    }
  }

  async function saveMicrosoftClientId() {
    busy = true;
    error = '';
    try {
      bootstrap = await invoke<Bootstrap>('set_microsoft_client_id', { clientId: clientIdDraft });
      notice = 'Configuración pública de Microsoft guardada';
    } catch (reason) {
      error = String(reason);
    } finally {
      busy = false;
    }
  }

  async function loginMicrosoft() {
    busy = true;
    error = '';
    notice = 'Completa el inicio de sesión en el navegador que se abrió…';
    try {
      bootstrap = await invoke<Bootstrap>('login_microsoft');
      notice = `Sesión iniciada como ${bootstrap.microsoftProfile?.username ?? 'Minecraft'}`;
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
      notice = 'Se quitó la cuenta y su credencial del llavero del sistema';
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
      <span class="eyebrow nav-caption">PREFERENCIAS</span>
      <button class:current={activePage === 'settings'} onclick={() => (activePage = 'settings')}><span>⚙</span> Ajustes</button>
      <button class:current={activePage === 'support'} onclick={openSupport}><span>?</span> Soporte</button>
    </nav>

    <div class="sidebar-footer"><span class="status-light"></span> APLICACIÓN NATIVA <small>v0.1.0 · PREVIEW</small></div>
  </aside>

  <main>
    <header class="topbar">
      <div class="breadcrumbs">ETERNALCRAFT <span>/</span> {activePage === 'home' ? 'INICIO' : activePage === 'mods' ? 'BIBLIOTECA' : activePage === 'support' ? 'SOPORTE' : 'AJUSTES'}</div>
      <div class="topbar-right"><span class="online-indicator"></span> INSTALACIÓN LOCAL</div>
    </header>

    {#if loading}
      <div class="center-state"><span class="spinner"></span><p>Preparando tu espacio de juego…</p></div>
    {:else if error && !bootstrap}
      <div class="center-state error-state"><span class="state-icon">!</span><h1>No se pudo iniciar</h1><p>{error}</p><button class="button primary" onclick={refresh}>Reintentar</button></div>
    {:else if bootstrap && selected}
      {#if error}<div class="toast error-toast" role="alert">{error}</div>{/if}
      {#if notice}<div class="toast" role="status">{notice}</div>{/if}

      {#if activePage === 'home'}
        <section class="page home-page">
          <div class="page-heading"><div><span class="eyebrow">TU PRÓXIMA AVENTURA</span><h1>Elige tu mundo.</h1><p>Una biblioteca, distintas historias de EternalCraft.</p></div><span class="connection-pill"><i></i> Catálogo local</span></div>

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

          <div class="play-panel"><div class="play-copy"><span class="eyebrow">CARPETA DE JUEGO</span><h3>{isLinked(selected) ? 'Instancia vinculada' : bootstrap.installedProfiles[selected.id] ? 'Instancia de EternalCraft' : isDetected(selected) ? 'Instancia detectada' : 'Conecta tu instancia'}</h3><p>{bootstrap.installedProfiles[selected.id] && !isLinked(selected) ? bootstrap.managedGameDirectories[selected.id] : pathFor(selected) || 'Selecciona la carpeta de juego de esta serie para guardarla en el launcher.'}</p>{#if minecraftStatus.running}<span class="saved-chip process-chip">MINECRAFT EN EJECUCIÓN · {minecraftStatus.seriesId === selected.id ? 'ESTA SERIE' : minecraftStatus.seriesId?.toUpperCase()} · PID {minecraftStatus.pid}</span>{:else if minecraftStatus.exitSuccess === false}<span class="warning-chip process-chip">ÚLTIMA SESIÓN CERRÓ CON ERROR · CÓDIGO {minecraftStatus.exitCode ?? 'DESCONOCIDO'}</span>{/if}</div><div class="play-actions">{#if isDetected(selected)}<button class="button secondary" onclick={linkDetectedDirectory} disabled={busy || syncingPack}>Usar carpeta detectada</button>{/if}<button class="button secondary" onclick={selectDirectory} disabled={busy || syncingPack || minecraftStatus.running}>{isLinked(selected) ? 'Cambiar carpeta' : 'Seleccionar carpeta'}</button>{#if bootstrap.installedProfiles[selected.id]}<span class="saved-chip">FORGE INSTALADO</span>{:else}<button class="button secondary" onclick={installBase} disabled={installingSeries !== null || syncingPack || minecraftStatus.running} title="Instala una instancia aislada y descarga Java 17 de Mojang si hace falta">Instalar base de Forge y Java 17</button>{/if}{#if selected.packStatus === 'available'}<button class="button secondary" onclick={syncOfficialPack} disabled={syncingPack || minecraftStatus.running || !bootstrap.installedProfiles[selected.id] && !isLinked(selected)}>{syncingPack ? 'Actualizando mods…' : 'Instalar / actualizar pack'}</button>{#if bootstrap.microsoftProfile}<button class="button primary" onclick={launchMinecraft} disabled={busy || minecraftStatus.running || syncingPack || !bootstrap.installedProfiles[selected.id] && !isLinked(selected)}>{minecraftStatus.running ? 'Minecraft ejecutándose' : busy ? 'Preparando…' : 'Jugar'}</button>{/if}{:else}<button class="button primary" disabled title="El modpack de esta serie todavía no tiene archivos oficiales publicados">Pack no publicado</button>{/if}</div></div>
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
        <section class="page narrow-page"><div class="page-heading"><div><span class="eyebrow">DIAGNÓSTICO LOCAL</span><h1>Soporte</h1><p>Registro de instalación de Minecraft y Forge.</p></div><button class="button secondary" onclick={openSupport} disabled={logsLoading}>{logsLoading ? 'Leyendo…' : 'Actualizar registro'}</button></div><div class="settings-card log-card"><div class="inventory-path"><span class="eyebrow">ARCHIVO LOCAL</span><code>{bootstrap.logFile}</code></div><pre class="log-viewer" aria-live="polite">{launcherLogs}</pre></div><p class="privacy-note">Se muestran hasta 256 KB recientes del registro del launcher y el registro de Minecraft de la serie activa. Los archivos permanecen en este equipo.</p></section>
      {:else}
        <section class="page narrow-page"><div class="page-heading"><div><span class="eyebrow">CONFIGURACIÓN LOCAL</span><h1>Ajustes</h1><p>Preferencias guardadas en este equipo.</p></div></div><div class="settings-card">
          <div class="setting-row"><div><span class="eyebrow">CUENTA DE MINECRAFT</span><h2>{bootstrap.microsoftProfile?.username ?? 'Sin sesión iniciada'}</h2><p>{bootstrap.microsoftProfile ? `UUID ${bootstrap.microsoftProfile.uuid}` : 'Inicia sesión con Microsoft para jugar cuando el pack de una serie esté publicado.'}</p></div><div class="play-actions">{#if bootstrap.microsoftProfile}<button class="button secondary" onclick={logoutMicrosoft} disabled={busy}>Cerrar sesión</button>{:else}<button class="button primary" onclick={loginMicrosoft} disabled={busy || !bootstrap.microsoftClientId}>{busy ? 'Esperando navegador…' : 'Conectar cuenta Microsoft'}</button>{/if}</div></div>
          <div class="setting-row microsoft-registration-row"><div><span class="eyebrow">REGISTRO DE APLICACIÓN · MICROSOFT</span><h2>Client ID público</h2><p>Es el identificador público de EternalCraft en Microsoft Entra; no es una contraseña. Para conseguirlo:</p><ol class="setup-steps"><li>Abre <a href="https://entra.microsoft.com/#view/Microsoft_AAD_RegisteredApps/ApplicationsListBlade" target="_blank" rel="noreferrer">App registrations ↗</a> y crea un registro llamado <strong>EternalCraft Launcher</strong>.</li><li>En tipos de cuenta, permite <strong>cuentas personales de Microsoft</strong>.</li><li>En <strong>Authentication → Add a platform → Mobile and desktop applications</strong>, agrega <code>http://localhost</code> y habilita el flujo de cliente público.</li><li>En <strong>Overview</strong>, copia <strong>Application (client) ID</strong> y pégalo aquí. No crees ni compartas un client secret.</li></ol><p>El inicio de sesión usa el navegador del sistema y PKCE; el callback local puede usar un puerto dinámico.</p><a class="text-link" href="https://learn.microsoft.com/en-us/entra/identity-platform/scenario-desktop-overview" target="_blank" rel="noreferrer">Guía oficial de Microsoft para aplicaciones de escritorio ↗</a><input class="text-input" type="text" autocomplete="off" spellcheck="false" placeholder="xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx" bind:value={clientIdDraft} /></div><button class="button secondary" onclick={saveMicrosoftClientId} disabled={busy || clientIdDraft.trim() === (bootstrap.microsoftClientId ?? '')}>Guardar ID</button></div>
          <div class="setting-row appearance-row"><div><span class="eyebrow">APARIENCIA</span><h2>Identidad visual</h2><p>Elige un acento inspirado en tus series y un fondo local opcional.</p><div class="theme-options"><button class:theme-selected={bootstrap.themeId === 'series'} class="button secondary" onclick={() => saveTheme('series')}>Color de la serie</button><button class:theme-selected={bootstrap.themeId === 'siege'} class="button secondary" onclick={() => saveTheme('siege')}>SIEGE</button><button class:theme-selected={bootstrap.themeId === 'ghouls'} class="button secondary" onclick={() => saveTheme('ghouls')}>Ghouls</button></div></div><div class="play-actions"><button class="button secondary" onclick={chooseBackground}>Elegir fondo</button>{#if bootstrap.backgroundPath}<button class="button secondary" onclick={clearBackground}>Quitar fondo</button>{/if}</div></div><div class="setting-row memory-setting"><div><span class="eyebrow">MEMORIA JVM · MINECRAFT</span><h2>{(memoryDraft / 1024).toFixed(1)} GiB asignados</h2><p>RAM detectada: {(bootstrap.memory.totalMb / 1024).toFixed(1)} GiB · límite recomendado: {(bootstrap.memory.maxMb / 1024).toFixed(1)} GiB. Se aplica al próximo inicio.</p><input class="memory-slider" type="range" min={bootstrap.memory.minMb} max={bootstrap.memory.maxMb} step="512" aria-label="Memoria RAM asignada a Minecraft" value={memoryDraft} oninput={(event) => (memoryDraft = Number(event.currentTarget.value))} /></div><div class="play-actions"><button class="button secondary" onclick={saveMemoryLimit} disabled={busy || memoryDraft === bootstrap.memory.selectedMb}>Guardar</button>{#if bootstrap.memory.manuallySelected}<button class="button secondary" onclick={resetMemoryLimit} disabled={busy}>Automático</button>{/if}</div></div><div class="setting-row"><div><span class="eyebrow">JAVA · MINECRAFT 1.20.1</span><h2>{bootstrap.java.compatible ? `Java ${bootstrap.java.version}` : 'Java 17 no está listo'}</h2><p>{bootstrap.java.detail}{#if bootstrap.java.executable}<br/><code>{bootstrap.java.executable}</code>{/if}</p></div><div class="play-actions"><span class:saved-chip={bootstrap.java.compatible} class:warning-chip={!bootstrap.java.compatible}>{bootstrap.java.compatible ? 'COMPATIBLE' : 'REVISAR'}</span><button class="button secondary" onclick={selectJava} disabled={busy}>Elegir Java 17</button>{#if bootstrap.javaManuallySelected}<button class="button secondary" onclick={resetJava} disabled={busy}>Automático</button>{/if}<button class="button secondary" onclick={refreshJava} disabled={javaRefreshing}>{javaRefreshing ? 'Comprobando…' : 'Volver a comprobar'}</button></div></div><div class="setting-row"><div><span class="eyebrow">INSTANCIA · {selected.name}</span><h2>Directorio del juego</h2><p>{pathFor(selected) || 'Todavía no has vinculado una carpeta.'}</p></div><div class="play-actions">{#if isDetected(selected)}<button class="button secondary" onclick={linkDetectedDirectory} disabled={busy}>Vincular detectada</button>{/if}<button class="button secondary" onclick={selectDirectory} disabled={busy}>{isLinked(selected) ? 'Cambiar carpeta' : 'Elegir carpeta'}</button></div></div><div class="setting-row"><div><span class="eyebrow">CONFIGURACIÓN</span><h2>Archivo de preferencias</h2><p>{bootstrap.configDirectory}</p></div><span class="saved-chip">GUARDADO LOCAL</span></div><div class="setting-row"><div><span class="eyebrow">DIAGNÓSTICO</span><h2>Registros de instalación</h2><p>{bootstrap.logFile}</p></div><button class="button secondary" onclick={openSupport}>Ver registro</button></div></div><p class="privacy-note">Las carpetas detectadas se sugieren sin alterarlas; solo se vinculan después de que lo confirmes.</p></section>
      {/if}
    {/if}
  </main>
</div>

<script lang="ts">
  import { invoke } from '@tauri-apps/api/core';
  import type { Bootstrap, ModInventory, Series } from './lib/types';

  let bootstrap = $state<Bootstrap | null>(null);
  let selected = $derived.by(() => {
    const current = bootstrap;
    return current?.series.find((series) => series.id === current.activeSeriesId);
  });
  let activePage = $state<'home' | 'mods' | 'settings'>('home');
  let loading = $state(true);
  let busy = $state(false);
  let error = $state('');
  let notice = $state('');
  let javaRefreshing = $state(false);
  let modInventory = $state<ModInventory | null>(null);
  let modsLoading = $state(false);
  let modsError = $state('');

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

  async function refresh() {
    loading = true;
    error = '';
    try {
      bootstrap = await invoke<Bootstrap>('get_bootstrap');
      await loadMods(bootstrap.activeSeriesId);
    } catch (reason) {
      error = String(reason);
    } finally {
      loading = false;
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

<div class="shell" style:--accent={selected?.accent ?? '#d1a35b'}>
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
            <button class:active={series.id === bootstrap?.activeSeriesId} class="series-choice" onclick={() => chooseSeries(series.id)} disabled={busy}>
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
    </nav>

    <div class="sidebar-footer"><span class="status-light"></span> APLICACIÓN NATIVA <small>v0.1.0 · PREVIEW</small></div>
  </aside>

  <main>
    <header class="topbar">
      <div class="breadcrumbs">ETERNALCRAFT <span>/</span> {activePage === 'home' ? 'INICIO' : activePage === 'mods' ? 'BIBLIOTECA' : 'AJUSTES'}</div>
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

          <div class="hero-card" style:--hero-accent={selected.accent}>
            <div class="hero-noise"></div><div class="hero-content">
              <span class="eyebrow">ETERNALCRAFT ORIGINAL SERIES</span>
              <h2>{selected.name}</h2><p class="hero-subtitle">{selected.subtitle}</p>
              <p class="hero-description">{selected.description}</p>
              <div class="hero-meta"><div><small>MINECRAFT</small><strong>{selected.minecraftVersion}</strong></div><div><small>CARGADOR</small><strong>{selected.loader} {selected.loaderVersion}</strong></div><div><small>ESTADO DEL PACK</small><strong class="muted-status">Aún sin publicar</strong></div></div>
            </div>
            <div class="hero-side"><div class="series-emblem">{selected.id === 'siege' ? 'S' : 'G'}</div><span class="side-label">SERIE<br/>SELECCIONADA</span></div>
          </div>

          <div class="section-heading"><div><span class="eyebrow">UNIVERSOS</span><h2>Explora las series</h2></div><span class="quiet-count">{bootstrap.series.length} SERIES</span></div>
          <div class="series-grid">
            {#each bootstrap.series as series (series.id)}
              <button class:selected-card={series.id === bootstrap.activeSeriesId} class="series-card" style:--card-accent={series.accent} onclick={() => chooseSeries(series.id)} disabled={busy}>
                <span class="card-orbit"></span><span class="card-kicker">ETERNALCRAFT</span><strong>{series.name}</strong><span class="card-subtitle">{series.subtitle}</span><span class="card-bottom">{series.minecraftVersion} <b>·</b> {series.loader} <span>↗</span></span>
              </button>
            {/each}
          </div>

          <div class="play-panel"><div class="play-copy"><span class="eyebrow">CARPETA DE JUEGO</span><h3>{isLinked(selected) ? 'Instancia vinculada' : isDetected(selected) ? 'Instancia detectada' : 'Conecta tu instancia'}</h3><p>{pathFor(selected) || 'Selecciona la carpeta de juego de esta serie para guardarla en el launcher.'}</p></div><div class="play-actions">{#if isDetected(selected)}<button class="button secondary" onclick={linkDetectedDirectory} disabled={busy}>Usar carpeta detectada</button>{/if}<button class="button secondary" onclick={selectDirectory} disabled={busy}>{isLinked(selected) ? 'Cambiar carpeta' : 'Seleccionar carpeta'}</button><button class="button primary" disabled title="Esta serie todavía no tiene una versión oficial publicada">Próximamente</button></div></div>
          <p class="release-note">Las versiones estarán disponibles cuando se publiquen manifiestos y archivos oficiales en este repositorio.</p>
        </section>
      {:else if activePage === 'mods'}
        <section class="page narrow-page"><div class="page-heading"><div><span class="eyebrow">CONTENIDO DEL JUEGO · {selected.name}</span><h1>Biblioteca de mods</h1><p>Inventario local de JARs. No se modifican archivos desde esta vista.</p></div><button class="button secondary" onclick={() => loadMods(selected.id)} disabled={modsLoading}>{modsLoading ? 'Leyendo…' : 'Actualizar inventario'}</button></div>
          {#if modsError}<div class="toast error-toast" role="alert">{modsError}</div>{/if}
          {#if !isLinked(selected)}<div class="empty-card"><div class="empty-icon">▦</div><h2>Vincula una carpeta de juego</h2><p>Elige una instancia en Inicio para revisar sus mods. La lectura del inventario es solo de consulta.</p><button class="button secondary" onclick={() => (activePage = 'home')}>Ir a Inicio</button></div>
          {:else if modsLoading}<div class="center-state"><span class="spinner"></span><p>Leyendo inventario de mods…</p></div>
          {:else if modInventory}<div class="settings-card inventory-card"><div class="inventory-path"><span class="eyebrow">CARPETA REVISADA</span><code>{modInventory.modsDirectory}</code></div><div class="inventory-stats"><article><strong>{modInventory.loadedFromModsRoot.length}</strong><span>JAR en <code>mods/</code></span></article><article><strong>{modInventory.officialStore.length}</strong><span>Guardados en <code>mods/Oficiales/</code></span></article><article><strong>{modInventory.personalStore.length}</strong><span>Guardados en <code>mods/personales/</code></span></article></div><div class="inventory-warning"><b>Compatibilidad Forge:</b> Forge 1.20.1 escanea los archivos JAR directamente dentro de <code>mods/</code>. Los JAR dentro de subcarpetas se muestran aparte y no se cuentan como cargados.</div><div class="inventory-list"><h2>JAR que Forge encuentra en la raíz</h2>{#if modInventory.loadedFromModsRoot.length}<ul>{#each modInventory.loadedFromModsRoot as file (file.name)}<li><span>{file.name}</span><small>{formatBytes(file.sizeBytes)}</small></li>{/each}</ul>{:else}<p>No hay JAR directamente dentro de <code>mods/</code>.</p>{/if}</div><div class="inventory-list storage-list"><h2>Almacenamiento organizado</h2><div class="storage-columns"><div><h3>Oficiales</h3>{#if modInventory.officialStore.length}<ul>{#each modInventory.officialStore as file (file.name)}<li><span>{file.name}</span><small>{formatBytes(file.sizeBytes)}</small></li>{/each}</ul>{:else}<p>Sin JAR guardados aquí.</p>{/if}</div><div><h3>Personales</h3>{#if modInventory.personalStore.length}<ul>{#each modInventory.personalStore as file (file.name)}<li><span>{file.name}</span><small>{formatBytes(file.sizeBytes)}</small></li>{/each}</ul>{:else}<p>Sin JAR guardados aquí.</p>{/if}</div></div></div></div>
          {/if}</section>
      {:else}
        <section class="page narrow-page"><div class="page-heading"><div><span class="eyebrow">CONFIGURACIÓN LOCAL</span><h1>Ajustes</h1><p>Preferencias guardadas en este equipo.</p></div></div><div class="settings-card"><div class="setting-row"><div><span class="eyebrow">JAVA · MINECRAFT 1.20.1</span><h2>{bootstrap.java.compatible ? `Java ${bootstrap.java.version}` : 'Java 17 no está listo'}</h2><p>{bootstrap.java.detail}{#if bootstrap.java.executable}<br/><code>{bootstrap.java.executable}</code>{/if}</p></div><div class="play-actions"><span class:saved-chip={bootstrap.java.compatible} class:warning-chip={!bootstrap.java.compatible}>{bootstrap.java.compatible ? 'COMPATIBLE' : 'REVISAR'}</span><button class="button secondary" onclick={selectJava} disabled={busy}>Elegir Java 17</button>{#if bootstrap.javaManuallySelected}<button class="button secondary" onclick={resetJava} disabled={busy}>Automático</button>{/if}<button class="button secondary" onclick={refreshJava} disabled={javaRefreshing}>{javaRefreshing ? 'Comprobando…' : 'Volver a comprobar'}</button></div></div><div class="setting-row"><div><span class="eyebrow">INSTANCIA · {selected.name}</span><h2>Directorio del juego</h2><p>{pathFor(selected) || 'Todavía no has vinculado una carpeta.'}</p></div><div class="play-actions">{#if isDetected(selected)}<button class="button secondary" onclick={linkDetectedDirectory} disabled={busy}>Vincular detectada</button>{/if}<button class="button secondary" onclick={selectDirectory} disabled={busy}>{isLinked(selected) ? 'Cambiar carpeta' : 'Elegir carpeta'}</button></div></div><div class="setting-row"><div><span class="eyebrow">CONFIGURACIÓN</span><h2>Archivo de preferencias</h2><p>{bootstrap.configDirectory}</p></div><span class="saved-chip">GUARDADO LOCAL</span></div></div><p class="privacy-note">Las carpetas detectadas se sugieren sin alterarlas; solo se vinculan después de que lo confirmes.</p></section>
      {/if}
    {/if}
  </main>
</div>

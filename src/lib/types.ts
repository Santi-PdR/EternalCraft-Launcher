/home/Santipdr/.bashrc: line 47: /tmp/eternalcraft-cargo/env: No such file or directory
/home/Santipdr/.bash_profile: line 18: /tmp/eternalcraft-cargo/env: No such file or directory
export interface Series {
  id: string;
  name: string;
  subtitle: string;
  description: string;
  accent: string;
  minecraftVersion: string;
  loader: string;
  loaderVersion: string;
  packStatus: 'unpublished' | 'available';
}

export interface Bootstrap {
  series: Series[];
  activeSeriesId: string;
  gameDirectories: Record<string, string>;
  suggestedDirectories: Record<string, string>;
  configDirectory: string;
  logFile: string;
  themeId: string;
  backgroundPath: string | null;
  java: JavaStatus;
  javaManuallySelected: boolean;
  managedGameDirectories: Record<string, string>;
  installedProfiles: Record<string, string>;
  microsoftClientId: string | null;
  microsoftProfile: { username: string; uuid: string } | null;
  memory: MemoryStatus;
}

export interface MemoryStatus {
  totalMb: number;
  minMb: number;
  maxMb: number;
  selectedMb: number;
  manuallySelected: boolean;
}

export interface JavaStatus {
  executable: string | null;
  version: string | null;
  major: number | null;
  compatible: boolean;
  detail: string;
}

export interface ModFileEntry {
  name: string;
  sizeBytes: number;
}

export interface ModInventory {
  gameDirectory: string | null;
  modsDirectory: string | null;
  loadedFromModsRoot: ModFileEntry[];
  officialStore: ModFileEntry[];
  personalStore: ModFileEntry[];
}

export interface InstallProgress {
  seriesId: string;
  stage: string;
  message: string;
  completedFiles: number;
}

export interface PackSyncResult {
  seriesId: string;
  version: string;
  downloadedFiles: number;
  removedFiles: number;
}

export interface PackSyncProgress {
  seriesId: string;
  completedFiles: number;
  totalFiles: number;
  message: string;
}

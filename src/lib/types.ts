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
  catalogOnline: boolean;
  gameDirectories: Record<string, string>;
  suggestedDirectories: Record<string, string>;
  configDirectory: string;
  logFile: string;
  themeId: string;
  backgroundPath: string | null;
  backgroundPreset: string | null;
  java: JavaStatus;
  javaManuallySelected: boolean;
  managedGameDirectories: Record<string, string>;
  installedProfiles: Record<string, string>;
  microsoftLoginAvailable: boolean;
  githubDeveloperEnabled: boolean;
  developerGithubUser: string | null;
  microsoftProfile: { username: string; uuid: string } | null;
  offlineUsername: string | null;
  accountMode: 'microsoft' | 'offline' | null;
  memory: MemoryStatus;
}

export interface MemoryStatus {
  totalMb: number;
  minMb: number;
  maxMb: number;
  selectedMb: number;
  manuallySelected: boolean;
}

export interface MinecraftStatus {
  running: boolean;
  seriesId: string | null;
  pid: number | null;
  exitCode: number | null;
  exitSuccess: boolean | null;
}

export interface DeveloperLoginStatus {
  status: 'pending' | 'authorized' | 'signedOut' | 'expired' | 'denied' | 'failed';
  username: string | null;
  userCode: string | null;
  verificationUri: string | null;
  expiresInSeconds: number | null;
  intervalSeconds: number | null;
  message: string | null;
}

export interface PackSourceFile {
  name: string;
  sizeBytes: number;
  sha256: string;
  license: string | null;
  licenseStatus: 'recognized' | 'permissionRequired' | 'unknown';
  minecraftCompatibility: 'compatible' | 'incompatible' | 'unknown';
  minecraftVersionRange: string | null;
  forgeCompatibility: 'compatible' | 'incompatible' | 'unknown';
  forgeVersionRange: string | null;
}

export interface PackSourcePreview {
  seriesId: string;
  directory: string;
  files: PackSourceFile[];
  totalBytes: number;
  sourceFingerprint: string;
}

export interface PackPublishProgress {
  seriesId: string;
  completedFiles: number;
  totalFiles: number;
  message: string;
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

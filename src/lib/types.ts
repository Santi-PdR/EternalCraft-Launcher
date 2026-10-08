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
  java: JavaStatus;
  javaManuallySelected: boolean;
}

export interface JavaStatus {
  executable: string | null;
  version: string | null;
  major: number | null;
  compatible: boolean;
  detail: string;
}

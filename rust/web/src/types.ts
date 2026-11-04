export interface Setting {
  name: string,
  type: "toggle" | "value" | "range",
  description: string
}

export interface SettingValue extends Setting {
  value: number,
  type: "value"
}

export interface SettingRange extends Setting {
  value: [number, number],
  type: "range"
}

export interface SettingToggle extends Setting {
  value: boolean,
  type: "toggle"
}

export type Settings = (SettingValue | SettingRange | SettingToggle)[];

export interface UserDetails {
  osuId: number,
  osuUsername: string,
  osuAvatarUrl: string,
  twitchId: number,
  twitchUsername: string,
  twitchAvatarUrl: string,
  excludedUsers: string[],
  isLive: boolean,
  twitchBotAuthorized: boolean
}

export interface MigrationStatus {
  active: boolean,
  cutoffAt: string | null
}

export interface Beatmap {
  id: number
  accuracy: number
  ar: number
  beatmapset: Beatmapset
  beatmapset_id: number
  bpm: number
  checksum: string
  convert: boolean
  count_circles: number
  count_sliders: number
  count_spinners: number
  cs: number
  deleted_at: string | null
  difficulty_rating: number
  drain: number
  failtimes: Failtimes
  hit_length: number
  is_scoreable: boolean
  last_updated: string
  max_combo: number
  mode: string
  mode_int: number
  passcount: number
  playcount: number
  ranked: number
  status: string
  total_length: number
  url: string
  user_id: number
  version: string
  count: number
}

export interface Beatmapset {
  artist: string
  artist_unicode: string
  covers: Covers
  creator: string
  favourite_count: number
  hype: Hype
  id: number
  nsfw: boolean
  offset: number
  play_count: number
  preview_url: string
  source: string
  spotlight: boolean
  status: string
  title: string
  title_unicode: string
  track_id: any
  user_id: number
  video: boolean
  bpm?: number
  can_be_hyped: boolean
  deleted_at: any
  discussion_enabled: boolean
  discussion_locked: boolean
  is_scoreable: boolean
  last_updated: string
  legacy_thread_url: string
  nominations_summary: NominationsSummary
  ranked: number
  ranked_date: any
  storyboard: boolean
  submitted_date: string
  tags: string
  availability: Availability
  ratings: number[]
}

export interface Covers {
  cover: string
  "cover@2x": string
  card: string
  "card@2x": string
  list: string
  "list@2x": string
  slimcover: string
  "slimcover@2x": string
}

export interface Hype {
  current: number
  required: number
}

export interface NominationsSummary {
  current: number
  required: number
}

export interface Availability {
  download_disabled: boolean
  more_information: any
}

export interface Failtimes {
  exit: number[]
  fail: number[]
}

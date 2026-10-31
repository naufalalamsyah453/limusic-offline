<script lang="ts">
	// The Downloads page: music kept offline on this machine (Rust `download.rs`). A downloaded
	// track is a file on disk, so playing it goes through the same local-music path as the Local
	// tab — a `LOCAL:<path>` id the backend resolves without touching the network. The page also
	// shows every download still in flight, from the `download-progress` event.
	import { onMount } from 'svelte';
	import type { UnlistenFn } from '@tauri-apps/api/event';
	import { HugeiconsIcon } from '@hugeicons/svelte';
	import {
		Download04Icon,
		Delete02Icon,
		FolderOpenIcon,
		PlayIcon,
		RefreshIcon
	} from '@hugeicons/core-free-icons';
	import { open } from '@tauri-apps/plugin-dialog';
	import { Button } from '$lib/components/ui/button';
	import TrackRow from '$lib/components/TrackRow.svelte';
	import ErrorState from '$lib/components/ErrorState.svelte';
	import * as api from '$lib/api';
	import type { Download, DownloadProgress, SongItem } from '$lib/api';
	import { openPlayer, playback, toast } from '$lib/player.svelte';
	import { t } from '$lib/i18n.svelte';

	let downloads = $state<Download[]>([]);
	let progress = $state<Record<string, DownloadProgress>>({});
	let dir = $state('');
	let loading = $state(true);
	let error = $state<string | null>(null);
	const SOURCE = 'Downloads';

	async function load() {
		loading = true;
		error = null;
		try {
			[downloads, dir] = await Promise.all([api.listDownloads(), api.downloadDir()]);
		} catch (e) {
			error = String(e);
		} finally {
			loading = false;
		}
	}

	let unlisten: UnlistenFn | undefined;
	onMount(() => {
		load();
		// A download started before this page opened still shows up: `download_progress` seeds the
		// map, then the event keeps it current.
		api.downloadProgress().then((list) => {
			for (const p of list) progress[p.videoId] = p;
		});
		api.onDownloadProgress((p) => {
			progress = { ...progress, [p.videoId]: p };
			if (p.state === 'done') {
				// The row is only listed once its row exists in the DB, so reload to pick it up.
				api.listDownloads().then((d) => (downloads = d));
				toast.success(t('downloads.done', { title: p.title }));
			} else if (p.state === 'failed') {
				toast.error(t('downloads.failed', { title: p.title }));
			}
		}).then((fn) => (unlisten = fn));
		return () => unlisten?.();
	});

	// In-flight downloads, newest state first; the ones already stored are the rows below.
	const active = $derived(Object.values(progress).filter((p) => p.state === 'downloading'));

	// A downloaded file plays exactly like a local file: a `LOCAL:<path>` id, nothing YouTube-shaped.
	function asSong(d: Download): SongItem {
		return {
			video_id: `LOCAL:${d.path}`,
			title: d.title,
			artists: d.artists,
			album: d.album,
			thumbnail: d.thumbnail,
			duration: d.durationSecs ? fmtDuration(d.durationSecs) : undefined
		};
	}
	const songs = $derived(downloads.map(asSong));
	const nowId = $derived(playback.now?.videoId);

	function fmtDuration(secs: number): string {
		const h = Math.floor(secs / 3600);
		const m = Math.floor((secs % 3600) / 60);
		const s = Math.floor(secs % 60);
		return h
			? `${h}:${String(m).padStart(2, '0')}:${String(s).padStart(2, '0')}`
			: `${m}:${String(s).padStart(2, '0')}`;
	}
	function fmtBytes(n: number): string {
		if (n < 1024) return `${n} B`;
		const units = ['KB', 'MB', 'GB'];
		let v = n / 1024;
		let i = 0;
		while (v >= 1024 && i < units.length - 1) {
			v /= 1024;
			i++;
		}
		return `${v.toFixed(1)} ${units[i]}`;
	}

	function playAll(shuffle = false) {
		if (!songs.length) return;
		openPlayer();
		api.playPlaylist(songs, shuffle ? null : 0, undefined, SOURCE, shuffle);
	}

	async function remove(d: Download) {
		try {
			await api.deleteDownload(d.videoId);
			downloads = downloads.filter((x) => x.videoId !== d.videoId);
			toast.success(t('downloads.removed'));
		} catch (e) {
			toast.error(String(e));
		}
	}

	async function pickDir() {
		const picked = await open({ directory: true, multiple: false, title: t('downloads.pick_dir') });
		if (typeof picked !== 'string') return;
		await api.setDownloadDir(picked);
		dir = await api.downloadDir();
		toast.success(t('downloads.dir_set'));
	}
</script>

<div class="p-6">
	<div class="mb-6 flex items-center justify-between gap-3">
		<div class="min-w-0">
			<h1 class="flex items-center gap-2 font-heading text-2xl font-bold">
				<HugeiconsIcon icon={Download04Icon} class="h-6 w-6" /> {t('downloads.title')}
			</h1>
			<button
				class="mt-1 flex max-w-full items-center gap-1.5 truncate text-xs text-muted-foreground hover:text-foreground"
				title={dir}
				onclick={pickDir}
			>
				<HugeiconsIcon icon={FolderOpenIcon} class="h-3.5 w-3.5 shrink-0" />
				<span class="truncate">{dir || t('downloads.no_dir')}</span>
			</button>
		</div>
		<div class="flex shrink-0 items-center gap-2">
			<Button variant="ghost" size="sm" class="gap-2" onclick={load}>
				<HugeiconsIcon icon={RefreshIcon} class="h-4 w-4" /> {t('common.refresh')}
			</Button>
			<Button size="sm" class="gap-2 rounded-full" disabled={!songs.length} onclick={() => playAll(false)}>
				<HugeiconsIcon icon={PlayIcon} class="h-4 w-4" /> {t('common.play_all')}
			</Button>
		</div>
	</div>

	{#if active.length}
		<div class="mb-6 flex flex-col gap-2">
			<h2 class="font-heading text-sm font-semibold text-muted-foreground">
				{t('downloads.downloading')}
			</h2>
			{#each active as p (p.videoId)}
				{@const pct = p.total ? Math.min(100, Math.round((p.downloaded / p.total) * 100)) : 0}
				<div class="rounded-xl border bg-card/40 p-3">
					<div class="mb-2 flex items-center justify-between gap-3 text-sm">
						<span class="truncate font-medium">{p.title}</span>
						<span class="shrink-0 text-xs text-muted-foreground">
							{p.total
								? `${fmtBytes(p.downloaded)} / ${fmtBytes(p.total)}`
								: fmtBytes(p.downloaded)}
						</span>
					</div>
					<div class="h-1.5 w-full overflow-hidden rounded-full bg-muted">
						<div
							class="h-full rounded-full bg-primary transition-[width] duration-200 {p.total
								? ''
								: 'animate-pulse w-1/3'}"
							style={p.total ? `width: ${pct}%` : ''}
						></div>
					</div>
				</div>
			{/each}
		</div>
	{/if}

	{#if loading}
		<p class="text-sm text-muted-foreground">{t('common.loading')}</p>
	{:else if error}
		<ErrorState message={error} onRetry={load} />
	{:else if !downloads.length}
		<div class="flex flex-col items-center gap-2 py-16 text-center">
			<HugeiconsIcon icon={Download04Icon} class="h-10 w-10 text-muted-foreground" />
			<p class="text-sm text-muted-foreground">{t('downloads.empty')}</p>
			<p class="max-w-sm text-xs text-muted-foreground">{t('downloads.empty_hint')}</p>
		</div>
	{:else}
		<div class="content-in">
			{#each downloads as d, i (d.videoId)}
				<div class="group/row relative">
					<TrackRow
						song={asSong(d)}
						index={i}
						lazy
						active={`LOCAL:${d.path}` === nowId}
						onplay={() => {
							openPlayer();
							api.playPlaylist(songs, i, undefined, SOURCE);
						}}
					/>
					<!-- Delete sits at the row's right edge, over the duration column, on hover. -->
					<div class="absolute right-2 top-1/2 hidden -translate-y-1/2 items-center gap-1 group-hover/row:flex">
						<span class="mr-1 text-xs text-muted-foreground">{fmtBytes(d.size)}</span>
						<button
							class="flex h-7 w-7 items-center justify-center rounded-full text-muted-foreground transition hover:bg-destructive/10 hover:text-destructive"
							aria-label={t('downloads.remove')}
							onclick={() => remove(d)}
						>
							<HugeiconsIcon icon={Delete02Icon} class="h-4 w-4" />
						</button>
					</div>
				</div>
			{/each}
		</div>
	{/if}
</div>

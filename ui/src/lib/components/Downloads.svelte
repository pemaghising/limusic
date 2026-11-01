<script lang="ts">
	import { onMount } from 'svelte';
	import { Download01Icon, Folder01Icon, PlayIcon } from '@hugeicons/core-free-icons';
	import { HugeiconsIcon } from '@hugeicons/svelte';
	import { Button } from './ui/button';
	import { Input } from './ui/input';
	import TrackRow from './TrackRow.svelte';
	import ErrorState from './ErrorState.svelte';
	import * as api from '$lib/api';
	import { downloads, refreshDownloads, cancelCurrentDownload, bytesLabel, chooseDownloadFolder, openDownloadFolder } from '$lib/downloads.svelte';
	import { openPlayer, openAddToPlaylist, playback, toast } from '$lib/player.svelte';
	import { t } from '$lib/i18n.svelte';

	onMount(() => { void refreshDownloads(); });
	let query = $state('');
	let shown = $state(100);
	const songs = $derived(downloads.items.filter(d => `${d.song.title} ${d.song.artists}`.toLocaleLowerCase().includes(query.toLocaleLowerCase().trim())).map(d => d.song));
	const totalBytes = $derived(downloads.items.reduce((sum, d) => sum + d.bytes, 0));
	$effect(() => { query; shown = 100; });
	async function play(index: number) {
		openPlayer();
		try { await api.playPlaylist(songs, index, undefined, t('downloads.title')); }
		catch (error) { toast.error(String(error)); }
	}
</script>

<div class="flex flex-col gap-4">
	<div class="flex flex-wrap items-center gap-2 rounded-lg border bg-card px-4 py-3">
		<HugeiconsIcon icon={Folder01Icon} class="size-4 shrink-0 text-muted-foreground" />
		<div class="min-w-0 flex-1">
			<p class="text-xs text-muted-foreground">{t('downloads.folder')}</p>
			<p class="truncate text-sm" title={downloads.folder}>{downloads.folder}</p>
		</div>
		<Button variant="outline" size="sm" onclick={openDownloadFolder}>{t('downloads.open_folder')}</Button>
		<Button variant="outline" size="sm" onclick={chooseDownloadFolder}>{t('downloads.change_folder')}</Button>
	</div>
	{#if downloads.busy || downloads.batch}
		<div class="rounded-lg border bg-card p-4" role="status" aria-live="polite">
			{#if downloads.batch}
				<p class="mb-1 truncate text-sm font-medium">{t('downloads.batch_progress', { done: Math.min(downloads.batch.done + 1, downloads.batch.total), count: downloads.batch.total, label: downloads.batch.label })}</p>
			{/if}
			<div class="mb-2 flex items-center justify-between gap-3">
				<p class="text-sm {downloads.batch ? 'text-muted-foreground' : 'font-medium'}">{t('downloads.downloading')} {bytesLabel(downloads.received)}{downloads.total ? ` / ${bytesLabel(downloads.total)}` : ''}</p>
				<Button variant="outline" size="sm" onclick={cancelCurrentDownload}>{t('common.cancel')}</Button>
			</div>
			<progress class="h-1.5 w-full accent-primary" max={downloads.total ?? 1} value={downloads.total ? downloads.received : undefined} aria-label={t('downloads.downloading')}></progress>
		</div>
	{/if}
	{#if !downloads.loaded}
		<p class="text-sm text-muted-foreground">{t('downloads.loading')}</p>
	{:else if downloads.error}
		<ErrorState message={downloads.error} onRetry={refreshDownloads} />
	{:else if !downloads.items.length}
		<div class="flex flex-col items-center gap-3 py-10 text-center">
			<HugeiconsIcon icon={Download01Icon} class="size-8 text-muted-foreground" />
			<p class="font-medium">{t('downloads.empty')}</p>
			<p class="max-w-md text-sm text-muted-foreground">{t('downloads.empty_hint')}</p>
		</div>
	{:else}
		<div class="flex flex-wrap items-center justify-between gap-3">
			<p class="text-sm text-muted-foreground">{t('downloads.summary', { count: downloads.items.length, size: bytesLabel(totalBytes) })}</p>
			<Button size="sm" class="gap-2" disabled={!songs.length} onclick={() => play(0)}><HugeiconsIcon icon={PlayIcon} class="size-4" />{t('downloads.play_all')}</Button>
		</div>
		<Input bind:value={query} aria-label={t('downloads.search')} placeholder={t('downloads.search')} class="max-w-sm" />
		<p class="text-xs text-muted-foreground">{t('downloads.offline_hint')}</p>
		<div>
			{#each songs.slice(0, shown) as song, i (song.video_id)}
				<TrackRow {song} index={i} active={playback.now?.videoId === song.video_id} onplay={() => play(i)} onAdd={() => openAddToPlaylist(song)} />
			{/each}
		</div>
		{#if songs.length > shown}<Button variant="outline" onclick={() => (shown += 100)}>{t('downloads.more')}</Button>{/if}
		{#if !songs.length}<p class="text-sm text-muted-foreground">{t('downloads.no_matches')}</p>{/if}
	{/if}
</div>

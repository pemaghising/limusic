import { open, save } from '@tauri-apps/plugin-dialog';
import * as api from './api';
import { t } from './i18n.svelte';
import { toast } from './player.svelte';

export const downloads = $state({
	items: [] as api.SavedDownload[], loaded: false, error: '',
	busy: null as string | null, exporting: null as string | null,
	received: 0, total: null as number | null,
	/** A whole album or playlist on its way down, one song at a time. */
	batch: null as { label: string; done: number; total: number; failed: number } | null,
	folder: ''
});
export const savedDownload = (id: string) => downloads.items.find(d => d.song.video_id === id);
let revision = 0;
export async function refreshDownloads() {
	const current = ++revision;
	try {
		const items = await api.listDownloads();
		if (current === revision) { downloads.items = items; downloads.error = ''; }
	} catch (error) { if (current === revision) downloads.error = String(error); }
	finally { if (current === revision) downloads.loaded = true; }
}
export function initDownloads() {
	let alive = true;
	const cleanups: (() => void)[] = [];
	const register = (pending: Promise<() => void>) => pending.then(cleanup => {
		if (alive) cleanups.push(cleanup); else cleanup();
	}).catch(error => console.error('Download event bridge:', error));
	register(api.onDownloadStarted(id => { downloads.busy = id; downloads.received = 0; downloads.total = null; }));
	register(api.onDownloadProgress(p => { downloads.busy = p.video_id; downloads.received = p.received; downloads.total = p.total ?? null; }));
	register(api.onDownloadFinished(id => { if (downloads.busy === id) downloads.busy = null; void refreshDownloads(); }));
	register(api.onDownloadsChanged(() => { void refreshDownloads(); }));
	void refreshDownloads();
	void api.downloadFolder().then(f => { downloads.folder = f; }).catch(() => {});
	return () => { alive = false; cleanups.forEach(cleanup => cleanup()); };
}
async function ensureDownload(song: api.SongItem, playlist?: api.PlaylistSlot) {
	const existing = savedDownload(song.video_id);
	// A song already saved still goes to the backend for a playlist: it adds that folder's copy.
	if (existing && !playlist) return existing;
	if (downloads.busy) throw new Error(t('downloads.busy'));
	downloads.busy = song.video_id;
	downloads.received = 0;
	downloads.total = null;
	try {
		const saved = await api.downloadSong(song, playlist);
		downloads.items = [saved, ...downloads.items.filter(d => d.song.video_id !== song.video_id)];
		return saved;
	} finally { if (downloads.busy === song.video_id) downloads.busy = null; }
}
type Job = { song: api.SongItem; playlist?: api.PlaylistSlot };
let jobs: Job[] = [];
let working = false;
let stopped = false;
/** Queue a whole album or playlist. With `playlist`, each song also lands in that playlist's
 *  folder, numbered in playlist order. Songs already saved are not fetched again. */
export function downloadMany(songs: api.SongItem[], label: string, playlist?: string) {
	const items = songs.filter(s => s.video_id);
	if (!items.length) return;
	const total = items.length;
	jobs.push(...items.map((song, i) => ({ song, playlist: playlist ? { name: playlist, position: i + 1, total } : undefined })));
	if (downloads.batch) { downloads.batch.total += total; downloads.batch.label = label; }
	else downloads.batch = { label, done: 0, total, failed: 0 };
	toast.success(t('downloads.queued', { count: total, label }));
	void work();
}
async function work() {
	if (working) return;
	working = true;
	stopped = false;
	try {
		while (jobs.length && !stopped) {
			const job = jobs.shift()!;
			try { await ensureDownload(job.song, job.playlist); }
			catch (error) {
				if (String(error) === 'Download cancelled.') break;
				if (downloads.batch) downloads.batch.failed++;
			}
			if (downloads.batch) downloads.batch.done++;
		}
	} finally {
		const batch = downloads.batch;
		const cancelled = stopped || jobs.length > 0;
		jobs = [];
		working = false;
		downloads.batch = null;
		if (batch && !cancelled) {
			if (batch.failed) toast.error(t('downloads.batch_failed', { failed: batch.failed, count: batch.total, label: batch.label }));
			else toast.success(t('downloads.batch_done', { count: batch.total, label: batch.label }));
		}
	}
}
export async function chooseDownloadFolder() {
	try {
		const path = await open({ directory: true, title: t('downloads.choose_folder'), defaultPath: downloads.folder || undefined });
		if (typeof path !== 'string') return;
		downloads.folder = await api.setDownloadFolder(path);
		toast.success(t('downloads.folder_changed'));
	} catch (error) { toast.error(String(error)); }
}
export async function openDownloadFolder() {
	try { await api.openDownloadFolder(); } catch (error) { toast.error(String(error)); }
}
export async function saveOffline(song: api.SongItem) {
	// One transfer at a time: while an album or playlist is going, a single song joins its queue.
	if (working) { downloadMany([song], song.title); return; }
	try { await ensureDownload(song); toast.success(t('downloads.saved')); }
	catch (error) { if (String(error) !== 'Download cancelled.') toast.error(String(error)); }
}
export async function exportAudio(song: api.SongItem) {
	if (downloads.exporting) return;
	downloads.exporting = song.video_id;
	try {
		const saved = await ensureDownload(song);
		const extension = saved.file.split('.').at(-1) ?? 'm4a';
		const cleaned = `${song.artists} - ${song.title}`.replace(/[<>:"/\\|?*\x00-\x1f]/g, '_').trim();
		// Bound UTF-8 bytes, not UTF-16 units: long non-Latin titles must fit filesystem limits.
		let name = '';
		for (const character of cleaned) {
			if (new TextEncoder().encode(name + character).length > 180) break;
			name += character;
		}
		name ||= 'Audio';
		const path = await save({ title: t('downloads.export'), defaultPath: `${name}.${extension}`, filters: [{ name: t('downloads.audio_file'), extensions: [extension] }] });
		if (!path) return;
		await api.exportDownload(song.video_id, path);
		toast.success(t('downloads.exported'));
	} catch (error) { if (String(error) !== 'Download cancelled.') toast.error(String(error)); }
	finally { downloads.exporting = null; }
}
export async function forgetDownload(id: string) {
	try { await api.removeDownload(id); await refreshDownloads(); toast.success(t('downloads.removed')); }
	catch (error) { if (String(error) !== 'Download cancelled.') toast.error(String(error)); }
}
export async function cancelCurrentDownload() {
	// Cancel means the whole album or playlist, not just the song in flight.
	stopped = true;
	jobs = [];
	if (downloads.busy) {
		try { await api.cancelDownload(downloads.busy); }
		catch (error) { if (String(error) !== 'Download cancelled.') toast.error(String(error)); }
	}
}
export function bytesLabel(bytes: number) {
	return bytes >= 1024 * 1024 ? `${(bytes / (1024 * 1024)).toFixed(1)} MB` : `${Math.round(bytes / 1024)} KB`;
}

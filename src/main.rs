use chrono::Local;
use clap::Parser;
use dotenvy::dotenv;
use serenity::async_trait;
use serenity::all::GatewayIntents;
use serenity::model::id::{ChannelId, GuildId, UserId};
use serenity::model::voice::VoiceState;
use serenity::prelude::*;
use songbird::driver::{DecodeConfig, DecodeMode};
use songbird::events::{Event, EventContext, EventHandler as VoiceEventHandler};
use songbird::{CoreEvent, SerenityInit};
use std::collections::HashMap;
use std::fs;
use std::io::BufWriter;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;

/// コマンドライン引数
#[derive(Parser)]
#[command(name = "discord_recorder", about = "Discord ボイスチャンネルの録音ツール")]
struct Args {
    /// Discord Bot トークン
    #[arg(env = "DISCORD_TOKEN")]
    token: String,

    /// 録音対象のボイスチャンネル ID
    #[arg(env = "DISCORD_CHANNEL_ID")]
    channel_id: u64,

    /// 録音ファイルの出力ディレクトリ
    #[arg(short, long, env = "OUTPUT_DIR", default_value = "output")]
    output: PathBuf,

    /// カスタムステータスメッセージ
    #[arg(short, long, env = "CUSTOM_STATUS")]
    status: Option<String>,
}

/// バッファリング中の Opus フレーム
struct PendingFrame {
    data: Vec<u8>,
    absgp: u64,
}

/// ユーザーごとの Opus トラック
struct UserTrack {
    encoder: audiopus::coder::Encoder,
    ogg_writer: ogg::PacketWriter<'static, BufWriter<std::fs::File>>,
    packet_count: u64,
    pre_skip: u16,
    pending: Option<PendingFrame>,
}

impl UserTrack {
    /// 新しい Opus トラックを作成します。
    fn create(path: std::path::PathBuf, bitrate: i32) -> Result<Self, Box<dyn std::error::Error>> {
        let file = BufWriter::new(std::fs::File::create(path)?);
        let mut ogg_writer = ogg::PacketWriter::new(file);

        let mut encoder = audiopus::coder::Encoder::new(
            audiopus::SampleRate::Hz48000,
            audiopus::Channels::Stereo,
            audiopus::Application::Audio,
        )?;
        encoder.set_bitrate(audiopus::Bitrate::BitsPerSecond(bitrate))?;
        let lookahead = encoder.lookahead()? as u16;

        // OpusHead パケット (19 bytes)
        let mut head = Vec::with_capacity(19);
        head.extend_from_slice(b"OpusHead");
        head.push(1);
        head.push(2);
        head.extend_from_slice(&lookahead.to_le_bytes());
        head.extend_from_slice(&48000u32.to_le_bytes());
        head.extend_from_slice(&0i16.to_le_bytes());
        head.push(0);

        ogg_writer.write_packet(
            head,
            1,
            ogg::PacketWriteEndInfo::EndPage,
            0,
        )?;

        // OpusTags パケット
        let vendor = b"discord_recorder";
        let mut tags = Vec::new();
        tags.extend_from_slice(b"OpusTags");
        tags.extend_from_slice(&(vendor.len() as u32).to_le_bytes());
        tags.extend_from_slice(vendor);
        tags.extend_from_slice(&0u32.to_le_bytes());

        ogg_writer.write_packet(
            tags,
            1,
            ogg::PacketWriteEndInfo::EndPage,
            0,
        )?;

        Ok(Self {
            encoder,
            ogg_writer,
            packet_count: 0,
            pre_skip: lookahead,
            pending: None,
        })
    }

    /// 20ms 分の PCM を Opus にエンコードして Ogg に書き込みます。
    /// 最終フレームはバッファリングし、finalize() で EOS フラグ付きで書き出します。
    fn write_frame(&mut self, pcm: &[i16]) -> Result<(), Box<dyn std::error::Error>> {
        let mut output = vec![0u8; 4000];
        let len = self.encoder.encode(pcm, &mut output)?;
        output.truncate(len);

        let absgp = self.pre_skip as u64 + self.packet_count * 960;
        self.packet_count += 1;

        if let Some(prev) = self.pending.replace(PendingFrame { data: output, absgp }) {
            self.ogg_writer.write_packet(
                prev.data,
                1,
                ogg::PacketWriteEndInfo::NormalPacket,
                prev.absgp,
            )?;
        }
        Ok(())
    }

    /// 無音フレーム（ゼロ埋め）をエンコードして書き込みます。
    fn write_silent_frame(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        static ZERO_PCM: [i16; 1920] = [0; 1920];
        self.write_frame(&ZERO_PCM)
    }

    /// Ogg Opus ストリームを正しく閉じます。
    /// バッファリングしていた最終フレームに EOS フラグを付けて書き出します。
    fn finalize(mut self) -> Result<(), Box<dyn std::error::Error>> {
        if let Some(prev) = self.pending.take() {
            let absgp = self.pre_skip as u64 + self.packet_count * 960;
            self.ogg_writer.write_packet(
                prev.data,
                1,
                ogg::PacketWriteEndInfo::EndStream,
                absgp,
            )?;
        }
        Ok(())
    }
}

/// 一回の録音セッションを管理する構造体
struct RecordingSession {
    /// 出力ディレクトリのパス
    dir_path: PathBuf,
    /// SSRCからユーザーIDへのマップ
    ssrc_to_user: HashMap<u32, UserId>,
    /// ユーザーIDからSSRCへの逆引きマップ
    user_id_to_ssrc: HashMap<UserId, u32>,
    /// ユーザーIDから表示名へのマップ
    user_id_to_name: HashMap<UserId, String>,
    /// ユーザーごとの音声トラック
    tracks: HashMap<UserId, UserTrack>,
    /// 経過Tick数（20ms単位）
    tick_count: u64,
    /// Discord チャンネルのビットレート
    bitrate: i32,
}

impl RecordingSession {
    /// 新しい録音セッションを作成します。
    fn new(output_dir: &PathBuf, dir_name: &str, bitrate: i32) -> Self {
        let dir_path = output_dir.join(dir_name);
        fs::create_dir_all(&dir_path).unwrap();
        Self {
            dir_path,
            ssrc_to_user: HashMap::new(),
            user_id_to_ssrc: HashMap::new(),
            user_id_to_name: HashMap::new(),
            tracks: HashMap::new(),
            tick_count: 0,
            bitrate,
        }
    }

    /// 全トラックの Opus ファイルを確定して閉じます。
    fn finalize(mut self) {
        for (_, track) in self.tracks.drain() {
            if let Err(e) = track.finalize() {
                eprintln!("ファイルの確定に失敗しました: {}", e);
            }
        }
    }

    /// 録音セッションを開始します。
    async fn start(
        guild_id: GuildId,
        channel_id: ChannelId,
        ctx: &Context,
        session: &Arc<Mutex<Option<RecordingSession>>>,
        output_dir: &PathBuf,
    ) {
        let dir_name;
        {
            let mut guard = session.lock().await;
            if guard.is_some() {
                return;
            }
            dir_name = Local::now().format("%Y%m%d%H%M%S").to_string();

            let bitrate = channel_id.to_channel(&ctx.http).await
                .ok()
                .and_then(|c| c.guild().and_then(|gc| gc.bitrate.map(|b| b as i32)))
                .unwrap_or(64000);

            *guard = Some(RecordingSession::new(output_dir, &dir_name, bitrate));
        }

        let manager = songbird::get(ctx).await.expect("Songbirdの初期化に失敗").clone();
        let call = manager.get_or_insert(guild_id);
        {
            let mut handler = call.lock().await;
            handler.add_global_event(Event::Core(CoreEvent::SpeakingStateUpdate), Receiver {
                session: session.clone(),
                ctx: ctx.clone(),
            });
            handler.add_global_event(Event::Core(CoreEvent::VoiceTick), Receiver {
                session: session.clone(),
                ctx: ctx.clone(),
            });
        }
        match manager.join(guild_id, channel_id).await {
            Ok(call) => {
                let mut handler = call.lock().await;
                let _ = handler.mute(true).await;
            }
            Err(e) => {
                eprintln!("音声チャンネルへの参加に失敗: {:?}", e);
            }
        }
        println!("[{}] 録音セッションを開始しました。", dir_name);
    }

    /// 録音セッションを終了します。
    async fn end(self, ctx: &Context, guild_id: GuildId) {
        self.finalize();
        let manager = songbird::get(ctx).await.expect("Songbirdの初期化に失敗").clone();
        if let Some(call) = manager.get(guild_id) {
            let mut handler = call.lock().await;
            let _ = handler.mute(false).await;
        }
        let _ = manager.remove(guild_id).await;
    }
}

impl Drop for RecordingSession {
    fn drop(&mut self) {
        for (_, track) in self.tracks.drain() {
            if let Err(e) = track.finalize() {
                eprintln!("Drop内でのファイル確定に失敗: {}", e);
            }
        }
    }
}

/// Songbirdの音声イベントを受信するハンドラ
struct Receiver {
    /// 共有セッションへの参照
    session: Arc<Mutex<Option<RecordingSession>>>,
    /// Serenityコンテキスト（ユーザー名解決に使用）
    ctx: Context,
}

#[async_trait]
impl VoiceEventHandler for Receiver {
    /// 音声イベントを処理します。
    async fn act(&self, ctx: &EventContext<'_>) -> Option<Event> {
        let mut session_opt = self.session.lock().await;
        let Some(session) = session_opt.as_mut() else { return None; };

        match ctx {
            EventContext::SpeakingStateUpdate(speaking) => {
                let Some(voice_uid) = speaking.user_id else { return None; };
                let id = UserId::new(voice_uid.0);
                session.ssrc_to_user.insert(speaking.ssrc, id);
                session.user_id_to_ssrc.insert(id, speaking.ssrc);

                if !session.tracks.contains_key(&id) {
                    let name = match id.to_user(&self.ctx.http).await {
                        Ok(user) => user.name,
                        Err(_) => id.to_string(),
                    };
                    session.user_id_to_name.insert(id, name.clone());

                    let file_path = session.dir_path.join(format!("{}.opus", name));
                    let mut track = UserTrack::create(file_path, session.bitrate)
                        .map_err(|e| eprintln!("Opusファイル作成失敗: {}", e))
                        .ok()?;

                    let missing_frames = session.tick_count;
                    for _ in 0..missing_frames {
                        if let Err(e) = track.write_silent_frame() {
                            eprintln!("無音書き込み失敗: {}", e);
                            break;
                        }
                    }

                    session.tracks.insert(id, track);
                }
            }
            EventContext::VoiceTick(tick) => {
                session.tick_count += 1;

                for (user_id, track) in session.tracks.iter_mut() {
                    let ssrc = session.user_id_to_ssrc.get(user_id).copied();

                    let mut audio_written = false;
                    if let Some(ssrc) = ssrc {
                        if let Some(voice_data) = tick.speaking.get(&ssrc) {
                            if let Some(decoded) = &voice_data.decoded_voice {
                                if let Err(e) = track.write_frame(decoded) {
                                    eprintln!("音声書き込み失敗: {}", e);
                                }
                                audio_written = true;
                            }
                        }
                    }

                    if !audio_written {
                        if let Err(e) = track.write_silent_frame() {
                            eprintln!("無音書き込み失敗: {}", e);
                        }
                    }
                }
            }
            _ => {}
        }
        None
    }
}

/// Serenityのイベントを処理するハンドラ
struct BotHandler {
    /// 録音対象のボイスチャンネルID
    target_channel_id: ChannelId,
    /// 録音ファイルの出力ディレクトリ
    output_dir: PathBuf,
    /// 共有セッションへの参照
    session: Arc<Mutex<Option<RecordingSession>>>,
    /// カスタムステータスメッセージ
    custom_status: Option<String>,
}

impl BotHandler {
    /// ボイスチャンネルの状態を確認し、録音を開始または終了します。
    async fn check_and_manage_recording(&self, ctx: &Context, guild_id: GuildId) {
        let current_users = ctx.cache.guild(guild_id).map(|guild| {
            guild.voice_states.values()
                .filter(|vs| vs.channel_id == Some(self.target_channel_id) && vs.user_id != ctx.cache.current_user().id)
                .count()
        }).unwrap_or(0);

        if current_users > 0 {
            RecordingSession::start(
                guild_id,
                self.target_channel_id,
                ctx,
                &self.session,
                &self.output_dir,
            ).await;
        } else {
            let mut guard = self.session.lock().await;
            if let Some(session) = guard.take() {
                drop(guard);
                session.end(ctx, guild_id).await;
            }
        }
    }
}

#[async_trait]
impl EventHandler for BotHandler {
    /// Botが起動したときに呼ばれます。
    async fn ready(&self, ctx: Context, ready: serenity::model::gateway::Ready) {
        println!("Botが起動しました: {}", ready.user.name);

        if let Some(status_text) = &self.custom_status {
            use serenity::gateway::ActivityData;
            let activity = ActivityData::custom(status_text);
            ctx.set_presence(Some(activity), serenity::model::user::OnlineStatus::Online);
            println!("カスタムステータスを設定しました: {}", status_text);
        }

        for guild in ready.guilds {
            self.check_and_manage_recording(&ctx, guild.id).await;
        }
    }

    /// ボイス状態が変化したときに呼ばれます。
    async fn voice_state_update(&self, ctx: Context, old: Option<VoiceState>, new: VoiceState) {
        let Some(guild_id) = new.guild_id.or_else(|| old.and_then(|o| o.guild_id)) else { return; };
        self.check_and_manage_recording(&ctx, guild_id).await;
    }
}

#[tokio::main]
async fn main() {
    dotenv().ok();

    let args = Args::parse();
    let intents = GatewayIntents::GUILDS
        | GatewayIntents::GUILD_VOICE_STATES
        | GatewayIntents::GUILD_MEMBERS;

    let handler = BotHandler {
        target_channel_id: ChannelId::new(args.channel_id),
        output_dir: args.output,
        session: Arc::new(Mutex::new(None)),
        custom_status: args.status,
    };

    // 【最重要設定】Songbird内部で自動的に復号化とPCMデコードを行う
    let songbird_config = songbird::Config::default()
        .decode_mode(DecodeMode::Decode(DecodeConfig::new(
            songbird::driver::Channels::Stereo,
            songbird::driver::SampleRate::Hz48000,
        )));

    let mut client = Client::builder(&args.token, intents)
        .event_handler(handler)
        .register_songbird_from_config(songbird_config)
        .await
        .expect("クライアントの作成に失敗しました");

    let shard_manager = client.shard_manager.clone();

    tokio::spawn(async move {
        tokio::signal::ctrl_c().await.expect("Ctrl+Cシグナルの受信に失敗");
        println!("\nシャットダウンシグナルを受信しました...");
        shard_manager.shutdown_all().await;
    });

    if let Err(why) = client.start().await {
        eprintln!("クライアントの実行エラー: {:?}", why);
    }
}

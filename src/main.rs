use serenity::async_trait;
use serenity::all::GatewayIntents;
use serenity::model::id::{ChannelId, GuildId, UserId};
use serenity::model::voice::VoiceState;
use serenity::prelude::*;
use songbird::events::{Event, EventContext, EventHandler as VoiceEventHandler};
use songbird::{CoreEvent, SerenityInit};
use songbird::driver::{DecodeConfig, DecodeMode};
use std::collections::HashMap;
use std::fs;
use std::io::BufWriter;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::Mutex;
use chrono::Local;

const SAMPLING_RATE: u32 = 48000;
const CHANNELS: u16 = 2;
const SAMPLES_PER_TICK: u64 = 1920; // 20ms * 48kHz (1chあたり960 * 2ch)

struct UserTrack {
    writer: hound::WavWriter<BufWriter<fs::File>>,
}

struct RecordingSession {
    dir_path: PathBuf,
    ssrc_to_user: HashMap<u32, UserId>,
    user_id_to_name: HashMap<UserId, String>,
    tracks: HashMap<UserId, UserTrack>,
    tick_count: u64, // 20msごとの絶対時間軸
}

impl RecordingSession {
    fn new(dir_name: &str) -> Self {
        let dir_path = PathBuf::from("output").join(dir_name);
        fs::create_dir_all(&dir_path).unwrap();
        Self {
            dir_path,
            ssrc_to_user: HashMap::new(),
            user_id_to_name: HashMap::new(),
            tracks: HashMap::new(),
            tick_count: 0,
        }
    }

    // セッション終了時に全ファイルのWAVヘッダを確定させて安全に閉じる
    fn finalize(self) {
        for (_, track) in self.tracks {
            let _ = track.writer.finalize();
        }
    }
}

struct Receiver {
    session: Arc<Mutex<Option<RecordingSession>>>,
    ctx: Context,
}

#[async_trait]
impl VoiceEventHandler for Receiver {
    async fn act(&self, ctx: &EventContext<'_>) -> Option<Event> {
        let mut session_opt = self.session.lock().await;
        let Some(session) = session_opt.as_mut() else { return None; };

        match ctx {
            EventContext::SpeakingStateUpdate(speaking) => {
                let Some(voice_uid) = speaking.user_id else { return None; };
                let id = UserId::new(voice_uid.0);
                session.ssrc_to_user.insert(speaking.ssrc, id);
                
                // 途中参加したユーザーのWAVファイルを作成し、開始からの経過時間分の無音を前詰めする
                if !session.tracks.contains_key(&id) {
                    let name = match id.to_user(&self.ctx.http).await {
                        Ok(user) => user.name,
                        Err(_) => id.to_string(),
                    };
                    session.user_id_to_name.insert(id, name.clone());
                    
                    let file_path = session.dir_path.join(format!("{}.wav", name));
                    let spec = hound::WavSpec {
                        channels: CHANNELS,
                        sample_rate: SAMPLING_RATE,
                        bits_per_sample: 16,
                        sample_format: hound::SampleFormat::Int,
                    };
                    let mut writer = hound::WavWriter::create(file_path, spec).unwrap();
                    
                    // 参加が遅れた分のTick（時間）をゼロ埋めして、DAW上の開始位置を強制同期させる
                    let missing_samples = session.tick_count * SAMPLES_PER_TICK;
                    for _ in 0..missing_samples {
                        writer.write_sample(0i16).unwrap();
                    }
                    
                    session.tracks.insert(id, UserTrack { writer });
                }
            }
            EventContext::VoiceTick(tick) => {
                session.tick_count += 1;

                // 20msの正確なメトロノームに合わせて、全員分のトラックに音声を書き込む
                for (user_id, track) in session.tracks.iter_mut() {
                    let ssrc = session.ssrc_to_user.iter()
                        .find(|(_, &id)| id == *user_id)
                        .map(|(&ssrc, _)| ssrc);
                    
                    let mut audio_written = false;
                    if let Some(ssrc) = ssrc {
                        // この20ms間にユーザーが発声していれば、デコード済みのPCMを取得
                        if let Some(voice_data) = tick.speaking.get(&ssrc) {
                            if let Some(decoded) = &voice_data.decoded_voice {
                                for &sample in decoded {
                                    track.writer.write_sample(sample).unwrap();
                                }
                                audio_written = true;
                            }
                        }
                    }

                    // 無音だった場合（またはパケットロス時）は強制的にゼロを書き込んで同期を維持する
                    if !audio_written {
                        for _ in 0..SAMPLES_PER_TICK {
                            track.writer.write_sample(0i16).unwrap();
                        }
                    }
                }
            }
            _ => {}
        }
        None
    }
}

struct BotHandler {
    target_channel_id: ChannelId,
    session: Arc<Mutex<Option<RecordingSession>>>,
    events_registered: AtomicBool,
}

impl BotHandler {
    async fn check_and_manage_recording(&self, ctx: &Context, guild_id: GuildId) {
        let manager = songbird::get(ctx).await.expect("Songbirdの初期化に失敗").clone();
        
        let current_users = ctx.cache.guild(guild_id).map(|guild| {
            guild.voice_states.values()
                .filter(|vs| vs.channel_id == Some(self.target_channel_id) && vs.user_id != ctx.cache.current_user().id)
                .count()
        }).unwrap_or(0);

        let mut session_lock = self.session.lock().await;

        if current_users > 0 && session_lock.is_none() {
            let dir_name = Local::now().format("%Y%m%d%H%M%S").to_string();
            *session_lock = Some(RecordingSession::new(&dir_name));
            
            if let Ok(handler_lock) = manager.join(guild_id, self.target_channel_id).await {
                let mut handler = handler_lock.lock().await;
                if !self.events_registered.swap(true, Ordering::Relaxed) {
                    handler.add_global_event(Event::Core(CoreEvent::SpeakingStateUpdate), Receiver { session: self.session.clone(), ctx: ctx.clone() });
                    handler.add_global_event(Event::Core(CoreEvent::VoiceTick), Receiver { session: self.session.clone(), ctx: ctx.clone() });
                }
            }
            println!("[{}] 録音セッションを開始しました。", dir_name);

        } else if current_users == 0 && session_lock.is_some() {
            if let Some(session) = session_lock.take() {
                session.finalize(); // 所有権を渡してWAVファイルを安全に閉じる
            }
            let _ = manager.leave(guild_id).await;
            println!("全員が退出したため、録音セッションを終了しました。");
        }
    }
}

#[async_trait]
impl EventHandler for BotHandler {
    async fn ready(&self, ctx: Context, ready: serenity::model::gateway::Ready) {
        println!("Botが起動しました: {}", ready.user.name);
        for guild in ready.guilds {
            self.check_and_manage_recording(&ctx, guild.id).await;
        }
    }

    async fn voice_state_update(&self, ctx: Context, old: Option<VoiceState>, new: VoiceState) {
        let Some(guild_id) = new.guild_id.or_else(|| old.and_then(|o| o.guild_id)) else { return; };
        self.check_and_manage_recording(&ctx, guild_id).await;
    }
}

#[tokio::main]
async fn main() {
    let token = "TOKEN_HERE";
    let target_channel: u64 = 0;
let intents = GatewayIntents::GUILDS 
        | GatewayIntents::GUILD_VOICE_STATES 
        | GatewayIntents::GUILD_MEMBERS;

    let handler = BotHandler {
        target_channel_id: ChannelId::new(target_channel),
        session: Arc::new(Mutex::new(None)),
        events_registered: AtomicBool::new(false),
    };

    // 【最重要設定】Songbird内部で自動的に復号化とPCMデコードを行う
    let songbird_config = songbird::Config::default()
        .decode_mode(DecodeMode::Decode(DecodeConfig::new(
            songbird::driver::Channels::Stereo,
            songbird::driver::SampleRate::Hz48000,
        )));

    let mut client = Client::builder(&token, intents)
        .event_handler(handler)
        .register_songbird_from_config(songbird_config)
        .await
        .expect("クライアントの作成に失敗しました");

    if let Err(why) = client.start().await {
        eprintln!("クライアントの実行エラー: {:?}", why);
    }
}

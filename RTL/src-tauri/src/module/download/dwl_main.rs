use crate::utils::request;
use futures::stream::{self, StreamExt};
use reqwest;
use sha1::Digest;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use std::env::consts::OS;

pub struct Download {
    pub version_manifest_url: String, // 获取版本url
    pub id: String,
    pub version_type: String,
}

pub struct DownloadOptions {
    pub url: String,        // 下载路径
    pub version_id: String, // 版本号
}

// 修改 DownloadInfo 结构体，添加下载进度跟踪
#[derive(Clone)]
struct DownloadInfo {
    url: String,
    path: std::path::PathBuf,
    size: u64,
    downloaded: Arc<AtomicUsize>,
}

// 添加新的结构体来跟踪下载进度
#[derive(Clone)]
struct DownloadProgress {
    total: Arc<AtomicUsize>,
    current: Arc<AtomicUsize>,
    success: Arc<AtomicUsize>,
    failed: Arc<AtomicUsize>,
}

impl DownloadProgress {
    fn new(total: usize) -> Self {
        Self {
            total: Arc::new(AtomicUsize::new(total)),
            current: Arc::new(AtomicUsize::new(0)),
            success: Arc::new(AtomicUsize::new(0)),
            failed: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn update_success(&self) {
        self.success.fetch_add(1, Ordering::SeqCst);
    }

    fn update_failed(&self) {
        self.failed.fetch_add(1, Ordering::SeqCst);
    }

    fn get_current(&self) -> usize {
        self.success.load(Ordering::SeqCst) + self.failed.load(Ordering::SeqCst)
    }
}

#[tauri::command]
pub async fn get_version_manifest() -> Result<serde_json::Value, String> {
    let download = Download::new(String::from(
        "https://piston-meta.mojang.com/mc/game/version_manifest.json",
    ));
    download
        .dwl_version_manifest()
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn dwl_version_manifest(url: String) -> Result<serde_json::Value, String> {
    let url = if url.starts_with('{') {
        // 如果输入是 JSON 字符串，尝试解析
        let parsed_json: serde_json::Value =
            serde_json::from_str(&url).map_err(|e| format!("JSON解析错误: {}", e))?;

        // 从 JSON 对象中提取 url 字段
        parsed_json
            .get("url")
            .and_then(|u| u.as_str())
            .ok_or("JSON中未找到有效的url字段")?
            .to_string()
    } else {
        // 如果输入是普通URL字符串，直接使用
        url
    };

    let download = DownloadOptions::new(url);
    let res = download
        .dwl_version_manifest()
        .await
        .map_err(|e| e.to_string())?;
    Ok(res)
}

impl Download {
    pub fn new(version_manifest_url: String) -> Self {
        Self {
            version_manifest_url: String::from(
                "https://piston-meta.mojang.com/mc/game/version_manifest.json",
            ),
            id: String::from(""),
            version_type: String::from(""),
        }
    }

    async fn dwl_version_manifest(&self) -> Result<serde_json::Value, Box<dyn std::error::Error>> {
        let request = request::Request::new(self.version_manifest_url.clone());
        let res = request.fetch_get().await.unwrap();
        let json_value = serde_json::from_str::<serde_json::Value>(&res)?;
        Ok(json_value)
    }
}

impl DownloadOptions {
    pub fn new(url: String) -> Self {
        Self {
            url,
            version_id: String::new(),
        }
    }

    // 下载游戏资源
    pub async fn dwl_version_manifest(
        &self,
    ) -> Result<serde_json::Value, Box<dyn std::error::Error + Send + Sync>> {
        let response = request::Request::new(self.url.clone());
        let res = response.fetch_get().await?;
        let mut timings = Vec::new();

        // 解析json
        let json_value: serde_json::Value = serde_json::from_str(&res)?;
        let version_id = json_value["id"].as_str().unwrap_or("unknown");

        // 创建主要目录
        let minecraft_path = std::path::Path::new(".minecraft");
        let version_path = minecraft_path.join("version").join(version_id);
        let libraries_path = minecraft_path.join("libraries");

        // 创建所需的目录
        std::fs::create_dir_all(&version_path)?;
        std::fs::create_dir_all(&libraries_path)?;

        let mut success_count = 0;
        let mut failed_count = 0;

        // 1. 客户端jar
        let jar_start = std::time::Instant::now();
        if let Some(client) = json_value
            .get("downloads")
            .and_then(|downloads| downloads.get("client"))
        {
            let client_url = client["url"].as_str().unwrap_or_default().to_string();
            let client_sha1 = client["sha1"].as_str().unwrap_or_default().to_string(); // 获取SHA1值
            let jar_path = version_path.join(format!("{}.jar", version_id));

            match download_and_verify_file(client_url, jar_path, &client_sha1, None, 3).await {
                Ok(info) => {
                    let duration = jar_start.elapsed();
                    timings.push(("客户端jar".to_string(), duration));
                    println!(
                        "✅ 下载成功: {} -> {} (耗时: {:.2}秒)",
                        info.url,
                        info.path.display(),
                        duration.as_secs_f64()
                    );
                    success_count += 1;
                }
                Err(e) => {
                    println!("❌ 下载失败: {}", e);
                    failed_count += 1;
                }
            }
        }

        // 创建两个异步任务，分别处理资源索引文件和libraries
        let assets_future = async {
            let assets_start = std::time::Instant::now();
            let mut result: Result<(), Box<dyn std::error::Error + Send + Sync>> = Ok(());

            if let Some(asset_index) = json_value.get("assetIndex") {
                if let Some(asset_url) = asset_index["url"].as_str() {
                    // 下载和解析资源索引文件
                    let asset_path = version_path.join("assets_index.json");
                    let response = request::Request::new(asset_url.to_string());
                    let asset_content = response.fetch_get().await?;
                    std::fs::write(&asset_path, &asset_content)?;

                    let asset_json: serde_json::Value = serde_json::from_str(&asset_content)?;

                    if let Some(objects) = asset_json.get("objects").and_then(|o| o.as_object()) {
                        let assets_path = std::path::Path::new(".minecraft").join("assets");
                        std::fs::create_dir_all(&assets_path)?;

                        // 准备下载任务
                        let download_tasks: Vec<(String, std::path::PathBuf, String)> = objects
                            .iter()
                            .filter_map(|(_, value)| {
                                let hash = value.get("hash").and_then(|h| h.as_str())?;
                                let hash_prefix = &hash[..2];
                                let download_url = format!(
                                    "https://resources.download.minecraft.net/{}/{}",
                                    hash_prefix, hash
                                );
                                let object_path =
                                    assets_path.join("objects").join(hash_prefix).join(hash);

                                if let Some(parent) = object_path.parent() {
                                    let _ = std::fs::create_dir_all(parent);
                                }

                                Some((download_url, object_path, hash.to_string()))
                            })
                            .collect();

                        let total_files = download_tasks.len();
                        let progress = DownloadProgress::new(total_files);
                        let failed_downloads = Arc::new(Mutex::new(Vec::new()));

                        println!("🚀 开始下载 {} 个资源文件...", total_files);

                        let batch_size = 250; // 控制并发量
                        let semaphore = Arc::new(tokio::sync::Semaphore::new(batch_size));

                        for chunk in download_tasks.chunks(batch_size) {
                            let mut futures = Vec::new();

                            for (url, path, expected_hash) in chunk {
                                let progress = progress.clone();
                                let failed_downloads = failed_downloads.clone();
                                let url = url.clone();
                                let path = path.clone();
                                let expected_hash = expected_hash.clone();
                                let permit = semaphore.clone().acquire_owned().await.unwrap();

                                futures.push(async move {
                                    let _permit = permit;
                                    let result = download_and_verify_file(
                                        url.clone(),
                                        path.clone(),
                                        &expected_hash,
                                        Some(progress.clone()),
                                        3,
                                    )
                                    .await;

                                    if let Err(e) = result {
                                        let mut failed = failed_downloads.lock().unwrap();
                                        failed.push((url, path));
                                        eprintln!("❌ 下载或验证失败: {}", e);
                                    }
                                });
                            }

                            // 使用stream进行并发控制
                            stream::iter(futures)
                                .buffer_unordered(batch_size) // 控制并发数
                                .collect::<Vec<_>>()
                                .await;

                            // 显示进度
                            let current = progress.get_current();
                            let total = progress.total.load(Ordering::SeqCst);
                            println!(
                                "📊 下载进度: {}/{} ({}%)",
                                current,
                                total,
                                (current as f32 / total as f32 * 100.0) as u32
                            );
                        }

                        // 处理失败的下载
                        let retry_list = failed_downloads.lock().unwrap().clone();
                        if !retry_list.is_empty() {
                            println!("🔄 重试 {} 个失败的下载...", retry_list.len());
                            for (url, path) in retry_list {
                                if let Err(e) =
                                    download_file_with_retry(url.clone(), path.clone(), None, 5)
                                        .await
                                {
                                    eprintln!("❌ 最终失败: {} -> {}", url, e);
                                    progress.update_failed();
                                } else {
                                    progress.update_success();
                                }
                            }
                        }

                        // 输出最终统计
                        let final_success = progress.success.load(Ordering::SeqCst);
                        let final_failed = progress.failed.load(Ordering::SeqCst);
                        println!("📊 下载完成:");
                        println!("✅ 成功: {} 个文件", final_success);
                        println!("❌ 失败: {} 个文件", final_failed);

                        if final_failed > 0 {
                            return Err("部分资源文件下载失败".into());
                        }

                        // 在资源下载完成后记录耗时
                        let duration = assets_start.elapsed();
                        timings.push(("资源索引文件".to_string(), duration));
                        println!(
                            "✅ 资源文件下载完成 (耗时: {:.2}秒)",
                            duration.as_secs_f64()
                        );
                    }
                }
            }

            result
        };

        let libraries_future = async {
            let libs_start = std::time::Instant::now();
            let mut success_count = 0;
            let mut failed_count = 0;

            if let Some(libraries) = json_value.get("libraries") {
                if let Some(libraries_array) = libraries.as_array() {
                    // 修改下载任务准备，加入SHA1
                    let download_tasks: Vec<_> = libraries_array
                        .iter()
                        .filter_map(|library| {
                            let downloads = library.get("downloads")?;
                            let artifact = downloads.get("artifact")?;
                            let url = artifact["url"].as_str()?;
                            let path = artifact.get("path").and_then(|p| p.as_str())?;
                            let sha1 = artifact["sha1"].as_str()?; // 获取SHA1值
                            let library_path = libraries_path.join(path);

                            if let Some(parent) = library_path.parent() {
                                let _ = std::fs::create_dir_all(parent);
                            }

                            Some((url.to_string(), library_path, sha1.to_string()))
                        })
                        .collect();

                    let total_libs = download_tasks.len();
                    let progress = DownloadProgress::new(total_libs);
                    let batch_size = 50;
                    let semaphore = Arc::new(tokio::sync::Semaphore::new(batch_size));

                    println!("🚀 开始下载 {} 个库文件...", total_libs);

                    // 修改下载流程，加入SHA1验证
                    stream::iter(download_tasks)
                        .map(|(url, path, sha1)| {
                            let semaphore = semaphore.clone();
                            let progress = progress.clone();

                            async move {
                                let _permit = semaphore.acquire().await.unwrap();
                                match download_and_verify_file(
                                    url.clone(),
                                    path.clone(),
                                    &sha1,
                                    Some(progress.clone()),
                                    3,
                                )
                                .await
                                {
                                    Ok(info) => {
                                        println!(
                                            "✅ 库文件下载成功: {} -> {}",
                                            info.url,
                                            info.path.display()
                                        );
                                        success_count += 1;
                                    }
                                    Err(e) => {
                                        println!("❌ 库文件下载失败: {} -> {}", url, e);
                                        failed_count += 1;
                                    }
                                }
                            }
                        })
                        .buffer_unordered(batch_size)
                        .collect::<Vec<_>>()
                        .await;

                    println!(
                        "📊 Libraries下载完成: 成功 {}, 失败 {}",
                        success_count, failed_count
                    );
                }
            }

            (success_count, failed_count, libs_start.elapsed())
        };

        // 并行执行两个任务
        let (assets_result, libraries_result) = tokio::join!(assets_future, libraries_future);

        // 处理结果
        let assets_result = assets_result?; // 处理 Result
        let (libs_success, libs_failed, libs_duration) = libraries_result;

        // 添加耗时统计
        timings.push(("Libraries".to_string(), libs_duration));

        // 3. 客户端映射文件 - 直接存储在版本目录中
        if let Some(client_mappings) = json_value
            .get("downloads")
            .and_then(|downloads| downloads.get("client_mappings"))
        {
            if let Some(mapping_url) = client_mappings["url"].as_str() {
                let mapping_path = version_path.join(format!("{}-mappings.txt", version_id));
                match download_file(mapping_url.to_string(), mapping_path).await {
                    Ok(info) => {
                        println!(
                            "✅ 映射文件下载成功: {} -> {}",
                            info.url,
                            info.path.display()
                        );
                        success_count += 1;
                    }
                    Err(e) => {
                        println!("❌ 映射文件下载失败: {}", e);
                        failed_count += 1;
                    }
                }
            }
        }

        // 输出所有资源的下载耗时统计
        println!("\n📊 下载耗时统计:");
        println!("----------------------------------------");
        for (resource, duration) in timings {
            println!("{}: {:.2}秒", resource, duration.as_secs_f64());
        }
        println!("----------------------------------------");

        println!(
            "📊 下载统计: 成功 {} 个文件, 失败 {} 个文件",
            success_count, failed_count
        );

        if failed_count > 0 {
            Err("部分文件下载失败".into())
        } else {
            Ok(json_value)
        }
    }
}

// 修改 DownloadInfo 结构体，添加下载进度跟踪
async fn download_with_progress(
    url: String,
    path: std::path::PathBuf,
    progress: Option<DownloadProgress>,
) -> Result<DownloadInfo, Box<dyn std::error::Error + Send + Sync>> {
    let client = reqwest::Client::new();
    let response = client.get(&url).send().await?;
    let total_size = response.content_length().unwrap_or(0);

    let downloaded = Arc::new(AtomicUsize::new(0));
    let file = tokio::fs::File::create(&path).await?;
    let mut writer = tokio::io::BufWriter::new(file);
    let mut stream = response.bytes_stream();

    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        writer.write_all(&chunk).await?;
        downloaded.fetch_add(chunk.len(), Ordering::SeqCst);
    }

    writer.flush().await?;

    Ok(DownloadInfo {
        url,
        path,
        size: total_size,
        downloaded,
    })
}

// 修改重试下载函数
async fn download_file_with_retry(
    url: String,
    path: std::path::PathBuf,
    progress: Option<DownloadProgress>,
    max_retries: u32,
) -> Result<DownloadInfo, Box<dyn std::error::Error + Send + Sync>> {
    let mut retries = 0;
    let mut last_error = None;

    while retries < max_retries {
        match download_with_progress(url.clone(), path.clone(), progress.clone()).await {
            Ok(info) => {
                // 验证文件大小
                if info.size > 0 && info.downloaded.load(Ordering::SeqCst) as u64 != info.size {
                    tokio::fs::remove_file(&path).await?;
                    retries += 1;
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    continue;
                }
                return Ok(info);
            }
            Err(e) => {
                last_error = Some(e);
                retries += 1;
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
    }

    Err(last_error.unwrap_or_else(|| "下载失败".into()))
}

// 下载文件
async fn download_file(
    url: String,
    path: std::path::PathBuf,
) -> Result<DownloadInfo, Box<dyn std::error::Error + Send + Sync>> {
    let response = request::Request::new(url.clone());
    let content = response.fetch_get().await?;

    let size = content.len() as u64;
    let path_clone = path.clone();
    tokio::task::spawn_blocking(move || std::fs::write(path_clone, content)).await??;

    Ok(DownloadInfo {
        url,
        path,
        size,
        downloaded: Arc::new(AtomicUsize::new(0)),
    })
}

// 用于文件验证
async fn download_and_verify_file(
    url: String,
    path: std::path::PathBuf,
    expected_hash: &str,
    progress: Option<DownloadProgress>,
    max_retries: u32,
) -> Result<DownloadInfo, Box<dyn std::error::Error + Send + Sync>> {
    let result =
        download_file_with_retry(url.clone(), path.clone(), progress.clone(), max_retries).await?;

    // 验证文件哈希
    let content = tokio::fs::read(&path).await?;
    let mut hasher = sha1::Sha1::new();
    hasher.update(&content);
    let actual_hash = format!("{:x}", hasher.finalize());

    if actual_hash != expected_hash {
        // 如果哈希值不匹配，删除文件并返回错误
        let _ = tokio::fs::remove_file(&path).await;
        if let Some(prog) = progress {
            prog.update_failed();
        }
        return Err(format!(
            "哈希值验证失败。期望：{}，实际：{}",
            expected_hash, actual_hash
        )
        .into());
    }

    if let Some(prog) = progress {
        prog.update_success();
    }

    Ok(result)
}

#[tokio::test]
pub async fn get_version_manifest_main() -> Result<(), String> {
    let version_manifest = Download::new(String::from(
        "https://piston-meta.mojang.com/mc/game/version_manifest.json",
    ));
    let latest_version = version_manifest.dwl_version_manifest().await.unwrap();
    println!("{}", latest_version);
    Ok(())
}

#[tokio::test]
pub async fn fetch_download_minecraft() -> Result<(), String> {
    let download = DownloadOptions::new(String::from(
        "https://piston-meta.moja…04ca1285508c/25w04a.json",
    ));
    let res = download.dwl_version_manifest().await.unwrap();
    println!("{}", res);
    Ok(())
}

#[tokio::test]
pub async fn get_system_info_main() -> Result<(), String> {
    println!("{:#?}", OS);
    Ok(())
}

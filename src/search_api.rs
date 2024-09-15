// Copyright (c) 2024 구FS, all rights reserved. Subject to the MIT licence in `licence.md`.
use crate::api_response::*;
use crate::error::*;
use crate::hentai::*;


/// # Summary
/// Searches nhentai.net for hentai with ID `hentai_id` and returns a corresponding HentaiTableRow entry. Updates database while doing so.
///
/// # Arguments
/// - `http_client`: reqwest http client
/// - `nhentai_hentai_search_url`: nhentai.net hentai search API URL
/// - `id`: hentai ID
/// - `db`: database connection
///
/// # Returns
/// - HentaiTableRow entry or error
pub async fn search_by_id(http_client: &reqwest::Client, nhentai_hentai_search_url: &str, id: u32, db: &sqlx::sqlite::SqlitePool) -> Result<HentaiTableRow, SearchByIdError>
{
    let r_serialised: HentaiSearchResponse; // response in json format


    let r: reqwest::Response = http_client.get(format!("{nhentai_hentai_search_url}{id}").as_str()).send().await?; // search hentai
    if r.status() != reqwest::StatusCode::OK {return Err(SearchByIdError::ReqwestStatus {url: r.url().to_string(), status: r.status()});} // if status is not ok: something went wrong
    // response in json format
    r_serialised = serde_json::from_str(r.text().await?.as_str())?; // deserialise json, get this response here to get number of pages before starting parallel workers
    if let Err(e) = r_serialised.write_to_db(db).await // save data to database, if unsuccessful: warning
    {
        log::warn!("Saving hentai \"{id}\" metadata in database failed with: {e}");
    }

    return Ok(HentaiTableRow
    {
        id: r_serialised.id,
        media_id: r_serialised.media_id,
        num_pages: r_serialised.num_pages,
        page_types: r_serialised.images.pages.iter().map(|page| format!("{:?}", page.t)).collect::<Vec<String>>().join(""),
        scanlator: r_serialised.scanlator,
        title_english: r_serialised.title.english,
        title_pretty: r_serialised.title.pretty,
        upload_date: r_serialised.upload_date,
    });
}


/// # Summary
/// Searches nhentai.net for all hentai ID with tags from `nhentai_tags` and returns them in a sorted list. Updates database while doing so.
///
/// # Arguments
/// - `http_client`: reqwest http client
/// - `nhentai_tag_search_url`: nhentai.net tag search API URL
/// - `nhentai_tags`: tags to search for
/// - `db`: database connection
///
/// # Returns
/// - list of hentai ID to download or error
pub async fn search_by_tag(http_client: &reqwest::Client, nhentai_tag_search_url: &str, nhentai_tags: &Vec<String>, db: &sqlx::sqlite::SqlitePool) -> Result<Vec<u32>, SearchByTagError>
{
    const WORKERS: usize = 2; // number of concurrent workers
    let f = scaler::Formatter::new()
        .set_scaling(scaler::Scaling::None)
        .set_rounding(scaler::Rounding::Magnitude(0)); // formatter
    let mut handles: Vec<tokio::task::JoinHandle<Option<Vec<u32>>>> = Vec::new(); // list of handles to tag_search_page
    let mut hentai_id_list: Vec<u32> = Vec::new(); // list of hentai id to download
    let r_serialised: TagSearchResponse; // response in json format
    let worker_sem: std::sync::Arc<tokio::sync::Semaphore> = std::sync::Arc::new(tokio::sync::Semaphore::new(WORKERS)); // limit number of concurrent workers otherwise api enforces rate limit


    {
        let r: reqwest::Response = http_client.get(format!("{nhentai_tag_search_url}?query={}&page=1", nhentai_tags.join("+"))).send().await?; // tag search, page, do not use .query() because it converts "+" between multiple tags to "%2B"
        log::debug!("{}", r.url());
        if r.status() != reqwest::StatusCode::OK {return Err(SearchByTagError::ReqwestStatus {url: r.url().to_string(), status: r.status()});} // if status is not ok: something went wrong
        r_serialised = serde_json::from_str(r.text().await?.as_str())?; // deserialise json, get this response here to get number of pages before starting parallel workers
        if let Err(e) = r_serialised.write_to_db(db).await // save data to database, if unsuccessful: warning
        {
            log::warn!("Saving hentai metadata page 1 / {} in database failed with: {e}", f.format(r_serialised.num_pages));
        }
        log::info!("Downloaded hentai metadata page 1 / {}.", f.format(r_serialised.num_pages));
    }

    for hentai in r_serialised.result // collect hentai id
    {
        hentai_id_list.push(hentai.id);
    }


    for page_no in 2..=r_serialised.num_pages // for each page, search in parallel
    {
        let db_clone: sqlx::Pool<sqlx::Sqlite> = db.clone();
        let f_clone: scaler::Formatter = f.clone();
        let http_client_clone: reqwest::Client = http_client.clone();
        let nhentai_tag_search_url_clone: String = nhentai_tag_search_url.to_owned();
        let nhentai_tags_clone: Vec<String> = nhentai_tags.to_owned();

        let permit: tokio::sync::OwnedSemaphorePermit = worker_sem.clone().acquire_owned().await.expect("Something closed semaphore even though it should never be closed."); // acquire semaphore
        handles.push(tokio::spawn(async move
        {
            let result: Option<Vec<u32>>;
            match search_by_tag_on_page(http_client_clone, nhentai_tag_search_url_clone, nhentai_tags_clone, page_no, r_serialised.num_pages, db_clone).await
            {
                Ok(o) =>
                {
                    log::info!("Downloaded hentai metadata page {} / {}.", f_clone.format(page_no), f_clone.format(r_serialised.num_pages));
                    result = Some(o);
                }
                Err(e) =>
                {
                    log::warn!("{e}");
                    result = None;
                }
            }
            drop(permit); // release semaphore
            result // return result into handle
        })); // search all pages in parallel
    }
    for handle in handles
    {
        if let Some(s) = handle.await.unwrap() {hentai_id_list.extend(s);} // collect results, forward panics
    }
    hentai_id_list.sort(); // sort hentai id ascending

    return Ok(hentai_id_list);
}


/// # Summary
/// Searches nhentai.net for all hentai ID with tag from `nhentai_tags` on page `page_no` and returns them in a list. Updates database while doing so.
///
/// # Arguments
/// - `http_client`: reqwest http client
/// - `nhentai_tag_search_url`: nhentai.net tag search api url
/// - `nhentai_tags`: tags to search for
/// - `page_no`: page number
/// - `db`: database connection
///
/// # Returns
/// - list of hentai ID to download or error
async fn search_by_tag_on_page(http_client: reqwest::Client, nhentai_tag_search_url: String, nhentai_tags: Vec<String>, page_no: u32, num_pages: u32, db: sqlx::sqlite::SqlitePool) -> Result<Vec<u32>, SearchByTagOnPageError>
{
    let f = scaler::Formatter::new()
        .set_scaling(scaler::Scaling::None)
        .set_rounding(scaler::Rounding::Magnitude(0)); // formatter
    let mut hentai_id_list: Vec<u32> = Vec::new(); // list of hentai id to download
    let mut r: reqwest::Response; // nhentai.net api response
    let r_serialised: TagSearchResponse; // response in json format+
    let r_text: String; // response text


    loop
    {
        match http_client.get(format!("{nhentai_tag_search_url}?query={}&page={page_no}", nhentai_tags.join("+"))).send().await // tag search, page, do not use .query() because it converts "+" between multiple tags to "%2B"
        {
            Ok(o) => r = o,
            Err(e) => return Err(SearchByTagOnPageError::Reqwest {page_no, num_pages, source: e}),
        }
        if r.status() == reqwest::StatusCode::TOO_MANY_REQUESTS // if status is too many requests: wait and retry
        {
            log::debug!("Downloading hentai metadata page {} from \"{}\" failed with status code {}. Waiting 2 s and retrying...", f.format(page_no), r.url().to_string(), r.status());
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            continue;
        }
        if r.status() != reqwest::StatusCode::OK {return Err(SearchByTagOnPageError::ReqwestStatus {page_no, num_pages, url: r.url().to_string(), status: r.status()});} // if status is not ok: something went wrong
        break; // everything went well, continue with processing
    }
    match r.text().await // get response text
    {
        Ok(o) => r_text = o,
        Err(e) => return Err(SearchByTagOnPageError::Reqwest {page_no, num_pages, source: e}),
    }
    match serde_json::from_str(r_text.as_str()) // deserialise json
    {
        Ok(o) => r_serialised = o,
        Err(e) => return Err(SearchByTagOnPageError::SerdeJson {page_no, num_pages, source: e}),
    }
    if let Err(e) = r_serialised.write_to_db(&db).await // save data to database
    {
        log::warn!("Saving hentai metadata page {} / {} in database failed with: {e}", f.format(page_no), f.format(num_pages));
    }

    for hentai in r_serialised.result // collect hentai id
    {
        hentai_id_list.push(hentai.id);
    }

    return Ok(hentai_id_list);
}
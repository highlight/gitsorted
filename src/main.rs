#[macro_use]
extern crate rocket;
extern crate octocrab;
extern crate postgrest;
extern crate sqlite;
use octocrab::models;
use serde::{Deserialize, Serialize};

use chrono::{DateTime, Utc};
use postgrest::Postgrest;
use std::time::Duration;

use rocket::{
    tokio::{task, time},
    Request, State,
};
use rocket_dyn_templates::{context, Template};

#[derive(Serialize, Deserialize, Debug)]
struct IssueObject {
    id: i32,
    number: i32,
    created_at: String,
    title: String,
}
// simple health check endpoint on /healthz
#[get("/healthz")]
fn healthz() -> &'static str {
    "OK"
}

#[get("/")]
async fn index(postgrest: &State<postgrest::Postgrest>) -> Template {
    let pg = postgrest.inner().clone();
    let resp = pg
        .from("Issues")
        .select("*")
        .execute()
        .await
        .expect("error reading from issues table")
        .text()
        .await
        .expect("error unwrapping text response");
    let parsed: Vec<IssueObject> = serde_json::from_str(&resp).expect("Failed to parse JSON");
    let res = Template::render(
        "index",
        context! {
            links: &parsed,
            title: "About"
        },
    );
    return res;
}

#[catch(500)]
fn internal_error(request: &Request) -> String {
    // Here you could inspect the request to log more details or take action.
    eprintln!("Internal Server Error: {:?}", request);
    // read the response body of the request
    format!("Sorry, '{}' is not a valid path.", request.uri())
}

#[rocket::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::init();
    octocrab::initialise(
        octocrab::Octocrab::builder()
            .personal_token(
                option_env!("GITHUB_TOKEN")
                    .ok_or("can't find github token")?
                    .to_string(),
            )
            .build()?,
    );

    let pg = Postgrest::new(
        option_env!("SUPABASE_URL")
            .ok_or("can't find supabase url")?
            .to_string(),
    )
    .insert_header(
        "apikey",
        option_env!("SUPABASE_TOKEN")
            .ok_or("error finding supabase token")?
            .to_string(),
    );

    let _forever = task::spawn(async {
        let mut interval = time::interval(Duration::from_millis(10000));
        loop {
            interval.tick().await;
            // handle the error from job_func gracefully
            if let Err(e) = job_func().await {
                log::error!("error running cron: {}", e);
            }
        }
    });

    let _rocket = rocket::build()
        .manage(pg)
        .mount("/", routes![index, healthz])
        .register("/", catchers![internal_error])
        .attach(Template::fairing())
        .launch()
        .await?;

    Ok(())
}

async fn job_func() -> Result<(), Box<dyn std::error::Error>> {
    // TODO, we shouldn't initialize supabase twice here. 
    let pg2 = Postgrest::new(
        option_env!("SUPABASE_URL")
            .ok_or("error finding supabase url")?
            .to_string(),
    )
    .insert_header(
        "apikey",
        option_env!("SUPABASE_TOKEN")
            .ok_or("error finding supabase token")?
            .to_string(),
    );
    // retrieve the most recent record in the issues db.
    let res = pg2
        .from("Issues")
        .select("*")
        .order("created_at.desc")
        .limit(1)
        .execute()
        .await?
        .text()
        .await?;
    log::info!("content of issues db: {}", res);
    let obj: Vec<IssueObject> =
        serde_json::from_str(res.as_str()).map_err(|e| format!("error parsing json: {}", e))?;
    if obj.len() != 1 {
        return Err("length of return value from supabase is not 1".into());
    }
    let datetime: DateTime<Utc> = obj[0].created_at.parse().map_err(|e| format!("error parsing datetime: {}", e))?;
    log::info!("latest datetime is: {}", datetime);
    let mut issue_vec: Vec<IssueObject> = Vec::new();
    let mut page = octocrab::instance()
        .issues("highlight", "highlight")
        .list()
        .state(octocrab::params::State::Open)
        .per_page(50)
        .send()
        .await?;
    loop {
        for issue in &page {
            if issue.created_at <= datetime {
                break;
            }
            issue_vec.push(IssueObject {
                number: issue.number.to_string().parse::<i32>()?,
                id: issue.id.to_string().parse::<i32>()?,
                created_at: issue.created_at.to_string(),
                title: issue.title.to_string(),
            });
        }
        page = match octocrab::instance()
            .get_page::<models::issues::Issue>(&page.next)
            .await?
        {
            Some(next_page) => next_page,
            None => break,
        }
    }
    log::info!("Found {:?} new issues to write", issue_vec.len());
    if issue_vec.len() != 0 {
        pg2.from("Issues")
            .insert(serde_json::to_string(&issue_vec)?)
            .execute()
            .await?
            .text()
            .await?;
    }
    Ok(())
}


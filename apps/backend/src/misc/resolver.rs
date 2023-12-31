use std::{collections::HashSet, sync::Arc};

use apalis::{prelude::Storage, sqlite::SqliteStorage};
use argon2::{Argon2, PasswordHash, PasswordVerifier};
use async_graphql::{Context, Enum, Error, InputObject, Object, Result, SimpleObject, Union};
use chrono::{Duration as ChronoDuration, Utc};
use cookie::{
    time::{Duration, OffsetDateTime},
    Cookie,
};
use futures::TryStreamExt;
use http::header::SET_COOKIE;
use rust_decimal::Decimal;
use sea_orm::{
    prelude::DateTimeUtc, ActiveModelTrait, ActiveValue, ColumnTrait, DatabaseConnection,
    EntityTrait, FromJsonQueryResult, ModelTrait, PaginatorTrait, QueryFilter, QueryOrder,
};
use sea_query::Order;
use serde::{Deserialize, Serialize};
use strum::IntoEnumIterator;
use uuid::Uuid;

use crate::{
    audio_books::{resolver::AudioBooksService, AudioBookSpecifics},
    background::UserCreatedJob,
    books::{resolver::BooksService, BookSpecifics},
    config::AppConfig,
    entities::{
        collection, media_import_report, metadata, metadata_to_collection,
        prelude::{
            Collection, MediaImportReport, Metadata, Review, Seen, Summary, User, UserToMetadata,
        },
        review, seen, summary, user, user_to_metadata,
        utils::{SeenExtraInformation, SeenShowExtraInformation},
    },
    graphql::{IdObject, Identifier},
    importer::ImportResultResponse,
    media::{
        resolver::{MediaDetails, MediaSearchItem, MediaService},
        MediaSpecifics, MetadataCreator, MetadataImage, MetadataImageUrl,
    },
    migrator::{
        MediaImportSource, MetadataImageLot, MetadataLot, MetadataSource, ReviewVisibility, UserLot,
    },
    movies::{resolver::MoviesService, MovieSpecifics},
    podcasts::{resolver::PodcastsService, PodcastSpecifics},
    shows::{resolver::ShowsService, ShowSpecifics},
    utils::{user_auth_token_from_ctx, user_id_from_ctx, MemoryDb, NamedObject},
    video_games::{resolver::VideoGamesService, VideoGameSpecifics},
};

use super::DefaultCollection;

pub static COOKIE_NAME: &str = "auth";

#[derive(Debug, Serialize, Deserialize, InputObject, Clone)]
pub struct CreateCustomMediaInput {
    pub title: String,
    pub lot: MetadataLot,
    pub description: Option<String>,
    pub creators: Option<Vec<String>>,
    pub genres: Option<Vec<String>>,
    pub images: Option<Vec<String>>,
    pub publish_year: Option<i32>,
    pub book_specifics: Option<BookSpecifics>,
    pub movie_specifics: Option<MovieSpecifics>,
    pub show_specifics: Option<ShowSpecifics>,
    pub video_game_specifics: Option<VideoGameSpecifics>,
    pub audio_book_specifics: Option<AudioBookSpecifics>,
    pub podcast_specifics: Option<PodcastSpecifics>,
}

#[derive(Enum, Clone, Debug, Copy, PartialEq, Eq)]
enum CreateCustomMediaErrorVariant {
    LotDoesNotMatchSpecifics,
}

#[derive(Debug, SimpleObject)]
struct CreateCustomMediaError {
    error: CreateCustomMediaErrorVariant,
}

#[derive(Union)]
enum CreateCustomMediaResult {
    Ok(IdObject),
    Error(CreateCustomMediaError),
}

#[derive(Enum, Clone, Debug, Copy, PartialEq, Eq)]
pub enum UserDetailsErrorVariant {
    AuthTokenInvalid,
}

#[derive(Debug, SimpleObject)]
pub struct UserDetailsError {
    error: UserDetailsErrorVariant,
}

#[derive(Union)]
pub enum UserDetailsResult {
    Ok(user::Model),
    Error(UserDetailsError),
}

#[derive(Debug, InputObject)]
struct UserInput {
    username: String,
    #[graphql(secret)]
    password: String,
}

#[derive(Enum, Clone, Debug, Copy, PartialEq, Eq)]
enum RegisterErrorVariant {
    UsernameAlreadyExists,
}

#[derive(Debug, SimpleObject)]
struct RegisterError {
    error: RegisterErrorVariant,
}

#[derive(Union)]
enum RegisterResult {
    Ok(IdObject),
    Error(RegisterError),
}

#[derive(Enum, Clone, Debug, Copy, PartialEq, Eq)]
enum LoginErrorVariant {
    UsernameDoesNotExist,
    CredentialsMismatch,
    MutexError,
}

#[derive(Debug, SimpleObject)]
struct LoginError {
    error: LoginErrorVariant,
}

#[derive(Debug, SimpleObject)]
struct LoginResponse {
    api_key: String,
}

#[derive(Union)]
enum LoginResult {
    Ok(LoginResponse),
    Error(LoginError),
}

#[derive(Debug, InputObject)]
struct UpdateUserInput {
    username: Option<String>,
    email: Option<String>,
    #[graphql(secret)]
    password: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ExportMedia {
    ryot_id: i32,
    title: String,
    #[serde(rename = "type")]
    lot: MetadataLot,
    audible_id: Option<String>,
    custom_id: Option<String>,
    goodreads_id: Option<String>,
    igdb_id: Option<String>,
    listennotes_id: Option<String>,
    openlibrary_id: Option<String>,
    tmdb_id: Option<String>,
    seen_history: Vec<seen::Model>,
    user_reviews: Vec<review::Model>,
}

fn create_cookie(
    ctx: &Context<'_>,
    api_key: &str,
    expires: bool,
    insecure_cookie: bool,
    token_valid_till: i32,
) -> Result<()> {
    let mut cookie = Cookie::build(COOKIE_NAME, api_key.to_string()).secure(!insecure_cookie);
    cookie = if expires {
        cookie.expires(OffsetDateTime::now_utc())
    } else {
        cookie
            .expires(OffsetDateTime::now_utc().checked_add(Duration::days(token_valid_till.into())))
    };
    let cookie = cookie.finish();
    ctx.insert_http_header(SET_COOKIE, cookie.to_string());
    Ok(())
}

fn get_hasher() -> Argon2<'static> {
    Argon2::default()
}

#[derive(
    SimpleObject, Debug, PartialEq, Eq, Clone, Default, Serialize, Deserialize, FromJsonQueryResult,
)]
pub struct AudioBooksSummary {
    runtime: i32,
    played: i32,
}

#[derive(
    SimpleObject, Debug, PartialEq, Eq, Clone, Default, Serialize, Deserialize, FromJsonQueryResult,
)]
pub struct VideoGamesSummary {
    played: i32,
}

#[derive(
    SimpleObject, Debug, PartialEq, Eq, Clone, Default, Serialize, Deserialize, FromJsonQueryResult,
)]
pub struct BooksSummary {
    pages: i32,
    read: i32,
}

#[derive(
    SimpleObject, Debug, PartialEq, Eq, Clone, Default, Serialize, Deserialize, FromJsonQueryResult,
)]
pub struct MoviesSummary {
    runtime: i32,
    watched: i32,
}

#[derive(
    SimpleObject, Debug, PartialEq, Eq, Clone, Default, Serialize, Deserialize, FromJsonQueryResult,
)]
pub struct PodcastsSummary {
    runtime: i32,
    played: i32,
    played_episodes: i32,
}

#[derive(
    SimpleObject, Debug, PartialEq, Eq, Clone, Default, Serialize, Deserialize, FromJsonQueryResult,
)]
pub struct ShowsSummary {
    runtime: i32,
    watched: i32,
    watched_episodes: i32,
    watched_seasons: i32,
}

#[derive(
    SimpleObject, Debug, PartialEq, Eq, Clone, Default, Serialize, Deserialize, FromJsonQueryResult,
)]
pub struct UserSummary {
    books: BooksSummary,
    movies: MoviesSummary,
    podcasts: PodcastsSummary,
    shows: ShowsSummary,
    video_games: VideoGamesSummary,
    audio_books: AudioBooksSummary,
}

#[derive(Debug, SimpleObject)]
struct ReviewPostedBy {
    id: Identifier,
    name: String,
}

#[derive(Debug, SimpleObject)]
struct ReviewItem {
    id: Identifier,
    posted_on: DateTimeUtc,
    rating: Option<Decimal>,
    text: Option<String>,
    visibility: ReviewVisibility,
    spoiler: bool,
    season_number: Option<i32>,
    episode_number: Option<i32>,
    posted_by: ReviewPostedBy,
    podcast_episode_id: Option<i32>,
}

#[derive(Debug, InputObject)]
pub struct PostReviewInput {
    pub rating: Option<Decimal>,
    pub text: Option<String>,
    pub visibility: Option<ReviewVisibility>,
    pub spoiler: Option<bool>,
    pub metadata_id: Identifier,
    pub date: Option<DateTimeUtc>,
    /// If this review comes from a different source, this should be set
    pub identifier: Option<String>,
    /// ID of the review if this is an update to an existing review
    pub review_id: Option<Identifier>,
    pub season_number: Option<i32>,
    pub episode_number: Option<i32>,
}

#[derive(Debug, SimpleObject)]
struct CollectionItem {
    collection_details: collection::Model,
    media_details: Vec<MediaSearchItem>,
}

#[derive(Debug, InputObject)]
pub struct AddMediaToCollection {
    pub collection_name: String,
    pub media_id: Identifier,
}

#[derive(Default)]
pub struct MiscQuery;

#[Object]
impl MiscQuery {
    /// Get all the public reviews for a media item.
    async fn media_item_reviews(
        &self,
        gql_ctx: &Context<'_>,
        metadata_id: Identifier,
    ) -> Result<Vec<ReviewItem>> {
        let user_id = user_id_from_ctx(gql_ctx).await?;
        gql_ctx
            .data_unchecked::<MiscService>()
            .media_item_reviews(&user_id, &metadata_id.into())
            .await
    }

    /// Get all collections for the currently logged in user.
    async fn collections(&self, gql_ctx: &Context<'_>) -> Result<Vec<CollectionItem>> {
        let user_id = user_id_from_ctx(gql_ctx).await?;
        gql_ctx
            .data_unchecked::<MiscService>()
            .collections(&user_id)
            .await
    }

    /// Get details about the currently logged in user.
    pub async fn user_details(&self, gql_ctx: &Context<'_>) -> Result<UserDetailsResult> {
        let token = user_auth_token_from_ctx(gql_ctx)?;
        gql_ctx
            .data_unchecked::<MiscService>()
            .user_details(&token)
            .await
    }

    /// Get a summary of all the media items that have been consumed by this user.
    pub async fn user_summary(&self, gql_ctx: &Context<'_>) -> Result<UserSummary> {
        let user_id = user_id_from_ctx(gql_ctx).await?;
        gql_ctx
            .data_unchecked::<MiscService>()
            .user_summary(&user_id)
            .await
    }
}

#[derive(Default)]
pub struct MiscMutation;

#[Object]
impl MiscMutation {
    /// Create or update a review.
    async fn post_review(&self, gql_ctx: &Context<'_>, input: PostReviewInput) -> Result<IdObject> {
        let user_id = user_id_from_ctx(gql_ctx).await?;
        gql_ctx
            .data_unchecked::<MiscService>()
            .post_review(&user_id, input)
            .await
    }

    /// Delete a review if it belongs to the user.
    async fn delete_review(&self, gql_ctx: &Context<'_>, review_id: Identifier) -> Result<bool> {
        let user_id = user_id_from_ctx(gql_ctx).await?;
        gql_ctx
            .data_unchecked::<MiscService>()
            .delete_review(&user_id, review_id.into())
            .await
    }

    /// Create a new collection for the logged in user.
    async fn create_collection(
        &self,
        gql_ctx: &Context<'_>,
        input: NamedObject,
    ) -> Result<IdObject> {
        let user_id = user_id_from_ctx(gql_ctx).await?;
        gql_ctx
            .data_unchecked::<MiscService>()
            .create_collection(&user_id, input)
            .await
    }

    /// Add a media item to a collection if it is not there, otherwise do nothing.
    async fn add_media_to_collection(
        &self,
        gql_ctx: &Context<'_>,
        input: AddMediaToCollection,
    ) -> Result<bool> {
        let user_id = user_id_from_ctx(gql_ctx).await?;
        gql_ctx
            .data_unchecked::<MiscService>()
            .add_media_to_collection(&user_id, input)
            .await
    }

    /// Remove a media item from a collection if it is not there, otherwise do nothing.
    async fn remove_media_from_collection(
        &self,
        gql_ctx: &Context<'_>,
        metadata_id: Identifier,
        collection_name: String,
    ) -> Result<IdObject> {
        let user_id = user_id_from_ctx(gql_ctx).await?;
        gql_ctx
            .data_unchecked::<MiscService>()
            .remove_media_item_from_collection(&user_id, &metadata_id.into(), &collection_name)
            .await
    }

    /// Delete a collection.
    async fn delete_collection(
        &self,
        gql_ctx: &Context<'_>,
        collection_name: String,
    ) -> Result<bool> {
        let user_id = user_id_from_ctx(gql_ctx).await?;
        gql_ctx
            .data_unchecked::<MiscService>()
            .delete_collection(&user_id, &collection_name)
            .await
    }

    /// Delete a seen item from a user's history.
    async fn delete_seen_item(
        &self,
        gql_ctx: &Context<'_>,
        seen_id: Identifier,
    ) -> Result<IdObject> {
        let user_id = user_id_from_ctx(gql_ctx).await?;
        gql_ctx
            .data_unchecked::<MiscService>()
            .delete_seen_item(seen_id.into(), user_id)
            .await
    }

    /// Deploy jobs to update all media item's metadata.
    async fn update_all_metadata(&self, gql_ctx: &Context<'_>) -> Result<bool> {
        gql_ctx
            .data_unchecked::<MiscService>()
            .update_all_metadata()
            .await
    }

    /// Create a new user for the service. Also set their `lot` as admin if
    /// they are the first user.
    async fn register_user(
        &self,
        gql_ctx: &Context<'_>,
        input: UserInput,
    ) -> Result<RegisterResult> {
        gql_ctx
            .data_unchecked::<MiscService>()
            .register_user(&input.username, &input.password)
            .await
    }

    /// Login a user using their username and password and return an API key.
    async fn login_user(&self, gql_ctx: &Context<'_>, input: UserInput) -> Result<LoginResult> {
        let config = gql_ctx.data_unchecked::<AppConfig>();
        let maybe_api_key = gql_ctx
            .data_unchecked::<MiscService>()
            .login_user(
                &input.username,
                &input.password,
                config.users.token_valid_for_days,
            )
            .await?;
        let config = gql_ctx.data_unchecked::<AppConfig>();
        if let LoginResult::Ok(LoginResponse { api_key }) = &maybe_api_key {
            create_cookie(
                gql_ctx,
                api_key,
                false,
                config.web.insecure_cookie,
                config.users.token_valid_for_days,
            )?;
        };
        Ok(maybe_api_key)
    }

    /// Logout a user from the server, deleting their login token.
    async fn logout_user(&self, gql_ctx: &Context<'_>) -> Result<bool> {
        let config = gql_ctx.data_unchecked::<AppConfig>();
        create_cookie(
            gql_ctx,
            "",
            true,
            config.web.insecure_cookie,
            config.users.token_valid_for_days,
        )?;
        let user_id = user_auth_token_from_ctx(gql_ctx)?;
        gql_ctx
            .data_unchecked::<MiscService>()
            .logout_user(&user_id)
            .await
    }

    /// Update a user's profile details.
    async fn update_user(&self, gql_ctx: &Context<'_>, input: UpdateUserInput) -> Result<IdObject> {
        let user_id = user_id_from_ctx(gql_ctx).await?;
        let config = gql_ctx.data_unchecked::<AppConfig>();
        gql_ctx
            .data_unchecked::<MiscService>()
            .update_user(&user_id, input, config)
            .await
    }

    /// Delete all summaries for the currently logged in user and then generate one from scratch.
    pub async fn regenerate_user_summary(&self, gql_ctx: &Context<'_>) -> Result<bool> {
        let user_id = user_id_from_ctx(gql_ctx).await?;
        gql_ctx
            .data_unchecked::<MiscService>()
            .regenerate_user_summary(user_id)
            .await
    }

    /// Create a custom media item.
    async fn create_custom_media(
        &self,
        gql_ctx: &Context<'_>,
        input: CreateCustomMediaInput,
    ) -> Result<CreateCustomMediaResult> {
        let user_id = user_id_from_ctx(gql_ctx).await?;
        gql_ctx
            .data_unchecked::<MiscService>()
            .create_custom_media(input, &user_id)
            .await
    }
}

#[derive(Debug, Clone)]
pub struct MiscService {
    db: DatabaseConnection,
    scdb: MemoryDb,
    media_service: Arc<MediaService>,
    audio_books_service: Arc<AudioBooksService>,
    books_service: Arc<BooksService>,
    movies_service: Arc<MoviesService>,
    podcasts_service: Arc<PodcastsService>,
    shows_service: Arc<ShowsService>,
    video_games_service: Arc<VideoGamesService>,
    user_created: SqliteStorage<UserCreatedJob>,
}

impl MiscService {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        db: &DatabaseConnection,
        scdb: &MemoryDb,
        media_service: &MediaService,
        audio_books_service: &AudioBooksService,
        books_service: &BooksService,
        movies_service: &MoviesService,
        podcasts_service: &PodcastsService,
        shows_service: &ShowsService,
        video_games_service: &VideoGamesService,
        user_created: &SqliteStorage<UserCreatedJob>,
    ) -> Self {
        Self {
            db: db.clone(),
            scdb: scdb.clone(),
            media_service: Arc::new(media_service.clone()),
            audio_books_service: Arc::new(audio_books_service.clone()),
            books_service: Arc::new(books_service.clone()),
            movies_service: Arc::new(movies_service.clone()),
            podcasts_service: Arc::new(podcasts_service.clone()),
            shows_service: Arc::new(shows_service.clone()),
            video_games_service: Arc::new(video_games_service.clone()),
            user_created: user_created.clone(),
        }
    }
}

impl MiscService {
    async fn media_item_reviews(
        &self,
        user_id: &i32,
        metadata_id: &i32,
    ) -> Result<Vec<ReviewItem>> {
        let all_reviews = Review::find()
            .order_by_desc(review::Column::PostedOn)
            .filter(review::Column::MetadataId.eq(metadata_id.to_owned()))
            .find_also_related(User)
            .all(&self.db)
            .await
            .unwrap()
            .into_iter()
            .map(|(r, u)| {
                let (show_se, show_ep, podcast_ep) = match r.extra_information {
                    Some(s) => match s {
                        SeenExtraInformation::Show(d) => (Some(d.season), Some(d.episode), None),
                        SeenExtraInformation::Podcast(d) => (None, None, Some(d.episode)),
                    },
                    None => (None, None, None),
                };
                let user = u.unwrap();
                ReviewItem {
                    id: r.id.into(),
                    posted_on: r.posted_on,
                    rating: r.rating,
                    spoiler: r.spoiler,
                    text: r.text,
                    visibility: r.visibility,
                    season_number: show_se,
                    episode_number: show_ep,
                    podcast_episode_id: podcast_ep,
                    posted_by: ReviewPostedBy {
                        id: user.id.into(),
                        name: user.name,
                    },
                }
            })
            .collect::<Vec<_>>();
        let all_reviews = all_reviews
            .into_iter()
            .filter(|r| match r.visibility {
                ReviewVisibility::Private => i32::from(r.posted_by.id) == *user_id,
                _ => true,
            })
            .collect();
        Ok(all_reviews)
    }

    async fn collections(&self, user_id: &i32) -> Result<Vec<CollectionItem>> {
        let collections = Collection::find()
            .filter(collection::Column::UserId.eq(user_id.to_owned()))
            .find_with_related(Metadata)
            .all(&self.db)
            .await
            .unwrap();
        let mut data = vec![];
        for (col, metas) in collections.into_iter() {
            let mut meta_data = vec![];
            for meta in metas {
                let m = self.media_service.generic_metadata(meta.id).await?;
                meta_data.push(MediaSearchItem {
                    identifier: m.model.id.to_string(),
                    lot: m.model.lot,
                    title: m.model.title,
                    images: m.poster_images,
                    publish_year: m.model.publish_year,
                })
            }
            data.push(CollectionItem {
                collection_details: col,
                media_details: meta_data,
            });
        }
        Ok(data)
    }

    pub async fn post_review(&self, user_id: &i32, input: PostReviewInput) -> Result<IdObject> {
        let meta = Review::find()
            .filter(review::Column::Identifier.eq(input.identifier.clone()))
            .one(&self.db)
            .await
            .unwrap();
        if let Some(m) = meta {
            Ok(IdObject {
                id: m.metadata_id.into(),
            })
        } else {
            let review_id = match input.review_id {
                Some(i) => ActiveValue::Set(i32::from(i)),
                None => ActiveValue::NotSet,
            };
            let mut review_obj = review::ActiveModel {
                id: review_id,
                rating: ActiveValue::Set(input.rating),
                text: ActiveValue::Set(input.text),
                user_id: ActiveValue::Set(user_id.to_owned()),
                metadata_id: ActiveValue::Set(i32::from(input.metadata_id)),
                extra_information: ActiveValue::NotSet,
                identifier: ActiveValue::Set(input.identifier),
                ..Default::default()
            };
            if let Some(s) = input.spoiler {
                review_obj.spoiler = ActiveValue::Set(s);
            }
            if let Some(v) = input.visibility {
                review_obj.visibility = ActiveValue::Set(v);
            }
            if let Some(d) = input.date {
                review_obj.posted_on = ActiveValue::Set(d);
            }
            if let (Some(s), Some(e)) = (input.season_number, input.episode_number) {
                review_obj.extra_information =
                    ActiveValue::Set(Some(SeenExtraInformation::Show(SeenShowExtraInformation {
                        season: s,
                        episode: e,
                    })));
            }
            let insert = review_obj.save(&self.db).await.unwrap();
            Ok(IdObject {
                id: insert.id.unwrap().into(),
            })
        }
    }

    pub async fn delete_review(&self, user_id: &i32, review_id: i32) -> Result<bool> {
        let review = Review::find()
            .filter(review::Column::Id.eq(review_id))
            .one(&self.db)
            .await
            .unwrap();
        match review {
            Some(r) => {
                if r.user_id == *user_id {
                    r.delete(&self.db).await?;
                    Ok(true)
                } else {
                    Err(Error::new("This review does not belong to you".to_owned()))
                }
            }
            None => Ok(false),
        }
    }

    pub async fn create_collection(&self, user_id: &i32, input: NamedObject) -> Result<IdObject> {
        let meta = Collection::find()
            .filter(collection::Column::Name.eq(input.name.clone()))
            .filter(collection::Column::UserId.eq(user_id.to_owned()))
            .one(&self.db)
            .await
            .unwrap();
        if let Some(m) = meta {
            Ok(IdObject { id: m.id.into() })
        } else {
            let col = collection::ActiveModel {
                name: ActiveValue::Set(input.name),
                user_id: ActiveValue::Set(user_id.to_owned()),
                ..Default::default()
            };
            let inserted = col
                .insert(&self.db)
                .await
                .map_err(|_| Error::new("There was an error creating the collection".to_owned()))?;
            Ok(IdObject {
                id: inserted.id.into(),
            })
        }
    }

    pub async fn delete_collection(&self, user_id: &i32, name: &str) -> Result<bool> {
        if DefaultCollection::iter().any(|n| n.to_string() == name) {
            return Err(Error::new("Can not delete a default collection".to_owned()));
        }
        let collection = Collection::find()
            .filter(collection::Column::Name.eq(name))
            .filter(collection::Column::UserId.eq(user_id.to_owned()))
            .one(&self.db)
            .await?;
        let resp = if let Some(c) = collection {
            Collection::delete_by_id(c.id).exec(&self.db).await.is_ok()
        } else {
            false
        };
        Ok(resp)
    }

    pub async fn remove_media_item_from_collection(
        &self,
        user_id: &i32,
        metadata_id: &i32,
        collection_name: &str,
    ) -> Result<IdObject> {
        let collect = Collection::find()
            .filter(collection::Column::Name.eq(collection_name.to_owned()))
            .filter(collection::Column::UserId.eq(user_id.to_owned()))
            .one(&self.db)
            .await
            .unwrap()
            .unwrap();
        let col = metadata_to_collection::ActiveModel {
            metadata_id: ActiveValue::Set(metadata_id.to_owned()),
            collection_id: ActiveValue::Set(collect.id),
        };
        let id = col.collection_id.clone().unwrap();
        col.delete(&self.db).await.ok();
        Ok(IdObject { id: id.into() })
    }

    pub async fn add_media_to_collection(
        &self,
        user_id: &i32,
        input: AddMediaToCollection,
    ) -> Result<bool> {
        let collection = Collection::find()
            .filter(collection::Column::UserId.eq(user_id.to_owned()))
            .filter(collection::Column::Name.eq(input.collection_name))
            .one(&self.db)
            .await
            .unwrap()
            .unwrap();
        let col = metadata_to_collection::ActiveModel {
            metadata_id: ActiveValue::Set(i32::from(input.media_id)),
            collection_id: ActiveValue::Set(collection.id),
        };
        Ok(col.clone().insert(&self.db).await.is_ok())
    }

    pub async fn start_import_job(
        &self,
        user_id: i32,
        source: MediaImportSource,
    ) -> Result<media_import_report::Model> {
        let model = media_import_report::ActiveModel {
            user_id: ActiveValue::Set(user_id),
            source: ActiveValue::Set(source),
            ..Default::default()
        };
        let model = model.insert(&self.db).await.unwrap();
        tracing::info!("Started import job with id = {id}", id = model.id);
        Ok(model)
    }

    pub async fn finish_import_job(
        &self,
        job: media_import_report::Model,
        details: ImportResultResponse,
    ) -> Result<media_import_report::Model> {
        let mut model: media_import_report::ActiveModel = job.into();
        model.finished_on = ActiveValue::Set(Some(Utc::now()));
        model.details = ActiveValue::Set(Some(details));
        model.success = ActiveValue::Set(Some(true));
        let model = model.update(&self.db).await.unwrap();
        Ok(model)
    }

    pub async fn media_import_reports(
        &self,
        user_id: i32,
    ) -> Result<Vec<media_import_report::Model>> {
        let reports = MediaImportReport::find()
            .filter(media_import_report::Column::UserId.eq(user_id))
            .all(&self.db)
            .await
            .unwrap();
        Ok(reports)
    }

    pub async fn delete_seen_item(&self, seen_id: i32, user_id: i32) -> Result<IdObject> {
        let seen_item = Seen::find_by_id(seen_id).one(&self.db).await.unwrap();
        if let Some(si) = seen_item {
            let seen_id = si.id;
            let progress = si.progress;
            let metadata_id = si.metadata_id;
            if si.user_id != user_id {
                return Err(Error::new(
                    "This seen item does not belong to this user".to_owned(),
                ));
            }
            si.delete(&self.db).await.ok();
            if progress < 100 {
                self.remove_media_item_from_collection(
                    &user_id,
                    &metadata_id,
                    &DefaultCollection::InProgress.to_string(),
                )
                .await
                .ok();
            }
            Ok(IdObject { id: seen_id.into() })
        } else {
            Err(Error::new("This seen item does not exist".to_owned()))
        }
    }

    pub async fn cleanup_summaries_for_user(&self, user_id: &i32) -> Result<()> {
        let summaries = Summary::find()
            .filter(summary::Column::UserId.eq(user_id.to_owned()))
            .all(&self.db)
            .await
            .unwrap();
        for summary in summaries.into_iter() {
            summary.delete(&self.db).await.ok();
        }
        Ok(())
    }

    pub async fn update_metadata(&self, metadata: metadata::Model) -> Result<()> {
        let metadata_id = metadata.id;
        tracing::info!("Updating metadata for {:?}", Identifier::from(metadata_id));
        let maybe_details = match metadata.lot {
            MetadataLot::AudioBook => {
                self.audio_books_service
                    .details_from_provider(metadata_id)
                    .await
            }
            MetadataLot::Book => self.books_service.details_from_provider(metadata_id).await,
            MetadataLot::Movie => self.movies_service.details_from_provider(metadata_id).await,
            MetadataLot::Podcast => {
                self.podcasts_service
                    .details_from_provider(metadata_id)
                    .await
            }
            MetadataLot::Show => self.shows_service.details_from_provider(metadata_id).await,
            MetadataLot::VideoGame => {
                self.video_games_service
                    .details_from_provider(metadata_id)
                    .await
            }
        };
        match maybe_details {
            Ok(details) => {
                self.media_service
                    .update_media(
                        metadata_id,
                        details.title,
                        details.description,
                        details.images,
                        details.creators,
                        details.specifics,
                        details.genres,
                    )
                    .await
                    .ok();
            }
            Err(e) => {
                tracing::error!("Error while updating: {:?}", e);
            }
        }

        Ok(())
    }

    pub async fn update_all_metadata(&self) -> Result<bool> {
        let metadatas = Metadata::find().all(&self.db).await.unwrap();
        for metadata in metadatas {
            self.media_service
                .deploy_update_metadata_job(metadata.id)
                .await?;
        }
        Ok(true)
    }

    async fn user_details(&self, token: &str) -> Result<UserDetailsResult> {
        let found_token = self.scdb.lock().unwrap().get(token.as_bytes()).unwrap();
        if let Some(t) = found_token {
            let user_id = std::str::from_utf8(&t).unwrap().parse::<i32>().unwrap();
            let user = User::find_by_id(user_id)
                .one(&self.db)
                .await
                .unwrap()
                .unwrap();
            Ok(UserDetailsResult::Ok(user))
        } else {
            Ok(UserDetailsResult::Error(UserDetailsError {
                error: UserDetailsErrorVariant::AuthTokenInvalid,
            }))
        }
    }

    async fn latest_user_summary(&self, user_id: &i32) -> Result<summary::Model> {
        let ls = Summary::find()
            .filter(summary::Column::UserId.eq(user_id.to_owned()))
            .order_by_desc(summary::Column::CreatedOn)
            .one(&self.db)
            .await
            .unwrap()
            .unwrap_or_default();
        Ok(ls)
    }

    async fn user_summary(&self, user_id: &i32) -> Result<UserSummary> {
        let ls = self.latest_user_summary(user_id).await?;
        Ok(ls.data)
    }

    pub async fn calculate_user_summary(&self, user_id: &i32) -> Result<IdObject> {
        let mut ls = summary::Model::default();
        let mut seen_items = Seen::find()
            .filter(seen::Column::UserId.eq(user_id.to_owned()))
            .filter(seen::Column::UserId.eq(user_id.to_owned()))
            .filter(seen::Column::Progress.eq(100))
            .find_also_related(Metadata)
            .stream(&self.db)
            .await?;

        let mut unique_shows = HashSet::new();
        let mut unique_show_seasons = HashSet::new();
        let mut unique_podcasts = HashSet::new();
        let mut unique_podcast_episodes = HashSet::new();
        while let Some((seen, metadata)) = seen_items.try_next().await.unwrap() {
            let meta = metadata.to_owned().unwrap();
            match meta.specifics {
                MediaSpecifics::AudioBook(item) => {
                    ls.data.audio_books.played += 1;
                    if let Some(r) = item.runtime {
                        ls.data.audio_books.runtime += r;
                    }
                }
                MediaSpecifics::Book(item) => {
                    ls.data.books.read += 1;
                    if let Some(pg) = item.pages {
                        ls.data.books.pages += pg;
                    }
                }
                MediaSpecifics::Podcast(item) => {
                    unique_podcasts.insert(seen.metadata_id);
                    for episode in item.episodes {
                        match seen.extra_information.to_owned() {
                            None => continue,
                            Some(sei) => match sei {
                                SeenExtraInformation::Show(_) => unreachable!(),
                                SeenExtraInformation::Podcast(s) => {
                                    if s.episode == episode.number {
                                        if let Some(r) = episode.runtime {
                                            ls.data.podcasts.runtime += r;
                                        }
                                        unique_podcast_episodes.insert((s.episode, episode.id));
                                    }
                                }
                            },
                        }
                    }
                }
                MediaSpecifics::Movie(item) => {
                    ls.data.movies.watched += 1;
                    if let Some(r) = item.runtime {
                        ls.data.movies.runtime += r;
                    }
                }
                MediaSpecifics::Show(item) => {
                    unique_shows.insert(seen.metadata_id);
                    for season in item.seasons {
                        for episode in season.episodes {
                            match seen.extra_information.to_owned().unwrap() {
                                SeenExtraInformation::Podcast(_) => unreachable!(),
                                SeenExtraInformation::Show(s) => {
                                    if s.season == season.season_number
                                        && s.episode == episode.episode_number
                                    {
                                        if let Some(r) = episode.runtime {
                                            ls.data.shows.runtime += r;
                                        }
                                        ls.data.shows.watched_episodes += 1;
                                        unique_show_seasons.insert((s.season, season.id));
                                    }
                                }
                            }
                        }
                    }
                }
                MediaSpecifics::VideoGame(_item) => {
                    ls.data.video_games.played += 1;
                }
                MediaSpecifics::Unknown => {}
            }
        }

        ls.data.podcasts.played += i32::try_from(unique_podcasts.len()).unwrap();
        ls.data.podcasts.played_episodes += i32::try_from(unique_podcast_episodes.len()).unwrap();

        ls.data.shows.watched = i32::try_from(unique_shows.len()).unwrap();
        ls.data.shows.watched_seasons += i32::try_from(unique_show_seasons.len()).unwrap();

        let summary_obj = summary::ActiveModel {
            id: ActiveValue::NotSet,
            created_on: ActiveValue::NotSet,
            user_id: ActiveValue::Set(user_id.to_owned()),
            data: ActiveValue::Set(ls.data),
        };
        let obj = summary_obj.insert(&self.db).await.unwrap();
        Ok(IdObject { id: obj.id.into() })
    }

    async fn register_user(&self, username: &str, password: &str) -> Result<RegisterResult> {
        let mut storage = self.user_created.clone();
        if User::find()
            .filter(user::Column::Name.eq(username))
            .count(&self.db)
            .await
            .unwrap()
            != 0
        {
            return Ok(RegisterResult::Error(RegisterError {
                error: RegisterErrorVariant::UsernameAlreadyExists,
            }));
        };
        let lot = if User::find().count(&self.db).await.unwrap() == 0 {
            UserLot::Admin
        } else {
            UserLot::Normal
        };
        let user = user::ActiveModel {
            name: ActiveValue::Set(username.to_owned()),
            password: ActiveValue::Set(password.to_owned()),
            lot: ActiveValue::Set(lot),
            ..Default::default()
        };
        let user = user.insert(&self.db).await.unwrap();
        storage
            .push(UserCreatedJob {
                user_id: user.id.into(),
            })
            .await?;
        Ok(RegisterResult::Ok(IdObject { id: user.id.into() }))
    }

    async fn login_user(
        &self,
        username: &str,
        password: &str,
        valid_for_days: i32,
    ) -> Result<LoginResult> {
        let user = User::find()
            .filter(user::Column::Name.eq(username))
            .one(&self.db)
            .await
            .unwrap();
        if user.is_none() {
            return Ok(LoginResult::Error(LoginError {
                error: LoginErrorVariant::UsernameDoesNotExist,
            }));
        };
        let user = user.unwrap();
        let parsed_hash = PasswordHash::new(&user.password).unwrap();
        if get_hasher()
            .verify_password(password.as_bytes(), &parsed_hash)
            .is_err()
        {
            return Ok(LoginResult::Error(LoginError {
                error: LoginErrorVariant::CredentialsMismatch,
            }));
        }
        let api_key = Uuid::new_v4().to_string();

        match self.scdb.lock() {
            Ok(mut d) => d.set(
                api_key.as_bytes(),
                user.id.to_string().as_bytes(),
                Some(
                    ChronoDuration::days(valid_for_days.into())
                        .num_seconds()
                        .try_into()
                        .unwrap(),
                ),
            )?,
            Err(e) => {
                tracing::error!("{:?}", e);
                return Ok(LoginResult::Error(LoginError {
                    error: LoginErrorVariant::MutexError,
                }));
            }
        };

        Ok(LoginResult::Ok(LoginResponse { api_key }))
    }

    async fn logout_user(&self, token: &str) -> Result<bool> {
        let found_token = self.scdb.lock().unwrap().get(token.as_bytes()).unwrap();
        if let Some(t) = found_token {
            self.scdb.lock().unwrap().delete(&t)?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    // this job is run when a user is created for the first time
    pub async fn user_created_job(&self, user_id: &i32) -> Result<()> {
        for collection in DefaultCollection::iter() {
            self.create_collection(
                user_id,
                NamedObject {
                    name: collection.to_string(),
                },
            )
            .await
            .ok();
        }
        Ok(())
    }

    async fn update_user(
        &self,
        user_id: &i32,
        input: UpdateUserInput,
        config: &AppConfig,
    ) -> Result<IdObject> {
        let mut user_obj: user::ActiveModel = User::find_by_id(user_id.to_owned())
            .one(&self.db)
            .await
            .unwrap()
            .unwrap()
            .into();
        if let Some(n) = input.username {
            if config.users.allow_changing_username {
                user_obj.name = ActiveValue::Set(n);
            }
        }
        if let Some(e) = input.email {
            user_obj.email = ActiveValue::Set(Some(e));
        }
        if let Some(p) = input.password {
            user_obj.password = ActiveValue::Set(p);
        }
        let user_obj = user_obj.update(&self.db).await.unwrap();
        Ok(IdObject {
            id: user_obj.id.into(),
        })
    }

    pub async fn regenerate_user_summaries(&self) -> Result<()> {
        let all_users = User::find().all(&self.db).await.unwrap();
        for user in all_users {
            self.calculate_user_summary(&user.id).await?;
        }
        Ok(())
    }

    pub async fn regenerate_user_summary(&self, user_id: i32) -> Result<bool> {
        self.cleanup_summaries_for_user(&user_id).await?;
        self.media_service
            .deploy_recalculate_summary_job(user_id)
            .await?;
        Ok(true)
    }

    async fn create_custom_media(
        &self,
        input: CreateCustomMediaInput,
        user_id: &i32,
    ) -> Result<CreateCustomMediaResult> {
        let mut input = input;
        let err = || {
            Ok(CreateCustomMediaResult::Error(CreateCustomMediaError {
                error: CreateCustomMediaErrorVariant::LotDoesNotMatchSpecifics,
            }))
        };
        let specifics = match input.lot {
            MetadataLot::AudioBook => match input.audio_book_specifics {
                None => return err(),
                Some(ref mut s) => MediaSpecifics::AudioBook(s.clone()),
            },
            MetadataLot::Book => match input.book_specifics {
                None => return err(),
                Some(ref mut s) => MediaSpecifics::Book(s.clone()),
            },
            MetadataLot::Movie => match input.movie_specifics {
                None => return err(),
                Some(ref mut s) => MediaSpecifics::Movie(s.clone()),
            },
            MetadataLot::Podcast => match input.podcast_specifics {
                None => return err(),
                Some(ref mut s) => MediaSpecifics::Podcast(s.clone()),
            },
            MetadataLot::Show => match input.show_specifics {
                None => return err(),
                Some(ref mut s) => MediaSpecifics::Show(s.clone()),
            },
            MetadataLot::VideoGame => match input.video_game_specifics {
                None => return err(),
                Some(ref mut s) => MediaSpecifics::VideoGame(s.clone()),
            },
        };
        let identifier = Uuid::new_v4().to_string();
        let images = input
            .images
            .unwrap_or_default()
            .into_iter()
            .map(|i| MetadataImage {
                url: MetadataImageUrl::S3(i),
                lot: MetadataImageLot::Poster,
            })
            .collect();
        let creators = input
            .creators
            .unwrap_or_default()
            .into_iter()
            .map(|c| MetadataCreator {
                name: c,
                role: "Creator".to_string(),
                image_urls: vec![],
            })
            .collect();
        let details = MediaDetails {
            identifier,
            title: input.title,
            description: input.description,
            lot: input.lot,
            source: MetadataSource::Custom,
            creators,
            genres: input.genres.unwrap_or_default(),
            images,
            publish_year: input.publish_year,
            publish_date: None,
            specifics,
        };
        let media = match input.lot {
            MetadataLot::AudioBook => self.audio_books_service.save_to_db(details).await?,
            MetadataLot::Book => self.books_service.save_to_db(details).await?,
            MetadataLot::Movie => self.movies_service.save_to_db(details).await?,
            MetadataLot::Podcast => self.podcasts_service.save_to_db(details).await?,
            MetadataLot::Show => self.shows_service.save_to_db(details).await?,
            MetadataLot::VideoGame => self.video_games_service.save_to_db(details).await?,
        };
        self.add_media_to_collection(
            user_id,
            AddMediaToCollection {
                collection_name: DefaultCollection::Custom.to_string(),
                media_id: media.id,
            },
        )
        .await?;
        Ok(CreateCustomMediaResult::Ok(media))
    }

    pub async fn json_export(&self, user_id: i32) -> Result<Vec<ExportMedia>> {
        let related_metadata = UserToMetadata::find()
            .filter(user_to_metadata::Column::UserId.eq(user_id))
            .all(&self.db)
            .await
            .unwrap();
        let distinct_meta_ids = related_metadata
            .into_iter()
            .map(|m| m.metadata_id)
            .collect::<Vec<_>>();
        let metas = Metadata::find()
            .filter(metadata::Column::Id.is_in(distinct_meta_ids))
            .order_by(metadata::Column::Id, Order::Asc)
            .all(&self.db)
            .await?;

        let mut resp = vec![];

        for m in metas {
            let seens = m
                .find_related(Seen)
                .filter(seen::Column::UserId.eq(user_id))
                .all(&self.db)
                .await
                .unwrap();
            let reviews = m
                .find_related(Review)
                .filter(review::Column::UserId.eq(user_id))
                .all(&self.db)
                .await
                .unwrap();
            let mut exp = ExportMedia {
                ryot_id: m.id,
                title: m.title,
                lot: m.lot,
                audible_id: None,
                custom_id: None,
                goodreads_id: None,
                igdb_id: None,
                listennotes_id: None,
                openlibrary_id: None,
                tmdb_id: None,
                seen_history: seens,
                user_reviews: reviews,
            };
            match m.source {
                MetadataSource::Audible => exp.audible_id = Some(m.identifier),
                MetadataSource::Custom => exp.custom_id = Some(m.identifier),
                MetadataSource::Goodreads => exp.goodreads_id = Some(m.identifier),
                MetadataSource::Igdb => exp.igdb_id = Some(m.identifier),
                MetadataSource::Listennotes => exp.listennotes_id = Some(m.identifier),
                MetadataSource::Openlibrary => exp.openlibrary_id = Some(m.identifier),
                MetadataSource::Tmdb => exp.tmdb_id = Some(m.identifier),
            };
            resp.push(exp);
        }

        Ok(resp)
    }
}

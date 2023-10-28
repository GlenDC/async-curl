use std::fmt::Debug;
#[cfg(feature = "multi")]
use std::thread::sleep;
#[cfg(feature = "multi")]
use std::time::Duration;

use curl::easy::{Easy2, Handler};
#[cfg(feature = "multi")]
use curl::multi::Multi;
use tokio::sync::mpsc::{self, Sender};
use tokio::sync::oneshot;

use crate::error::Error;
/// CurlActor is responsible for performing
/// the contructed Easy2 object by passing
/// it into send_request
/// ```
/// use curl::easy::Easy2;
/// use async_curl::response_handler::ResponseHandler;
/// use async_curl::actor::CurlActor;
///
/// # #[tokio::main(flavor = "current_thread")]
/// # async fn main() -> Result<(), Box<dyn std::error::Error>>{
/// let curl = CurlActor::new();
/// let mut easy2 = Easy2::new(ResponseHandler::new());
///
/// easy2.url("https://www.rust-lang.org").unwrap();
/// easy2.get(true).unwrap();
///
/// let response = curl.send_request(easy2).await.unwrap();
/// eprintln!("{:?}", response.get_ref());
///
/// Ok(())
/// # }
/// ```
///
/// Example for multiple request executed
/// at the same time.
///
/// ```
/// use async_curl::{actor::CurlActor, response_handler::ResponseHandler};
/// use curl::easy::Easy2;
///
/// # #[tokio::main(flavor = "current_thread")]
/// # async fn main() -> Result<(), Box<dyn std::error::Error>> {
/// let curl = CurlActor::new();
/// let mut easy2 = Easy2::new(ResponseHandler::new());
/// easy2.url("https://www.rust-lang.org").unwrap();
/// easy2.get(true).unwrap();
///
/// let spawn1 = tokio::spawn(async move {
///     let response = curl.send_request(easy2).await;
///     let mut response = response.unwrap();
///
///     // Response body
///     eprintln!(
///         "Task 1 : {}",
///         String::from_utf8_lossy(&response.get_ref().to_owned().get_data())
///     );
///     // Response status code
///     let status_code = response.response_code().unwrap();
///     eprintln!("Task 1 : {}", status_code);
/// });
///
/// let curl = CurlActor::new();
/// let mut easy2 = Easy2::new(ResponseHandler::new());
/// easy2.url("https://www.rust-lang.org").unwrap();
/// easy2.get(true).unwrap();
///
/// let spawn2 = tokio::spawn(async move {
///     let response = curl.send_request(easy2).await;
///     let mut response = response.unwrap();
///
///     // Response body
///     eprintln!(
///         "Task 2 : {}",
///         String::from_utf8_lossy(&response.get_ref().to_owned().get_data())
///     );
///     // Response status code
///     let status_code = response.response_code().unwrap();
///     eprintln!("Task 2 : {}", status_code);
/// });
/// let (_, _) = tokio::join!(spawn1, spawn2);
///
/// Ok(())
/// # }
/// ```
///
#[derive(Clone)]
pub struct CurlActor<H>
where
    H: Handler + Debug + Send + 'static,
{
    request_sender: Sender<Request<H>>,
}

impl<H> Default for CurlActor<H>
where
    H: Handler + Debug + Send + 'static,
{
    fn default() -> Self {
        Self::new()
    }
}

impl<H> CurlActor<H>
where
    H: Handler + Debug + Send + 'static,
{
    /// This creates the new instance of CurlActor.
    /// This spawns a new asynchronous task using tokio
    /// so that it won't block. The perform_curl
    /// function is executed when send_request is called
    pub fn new() -> Self {
        let (request_sender, mut request_receiver) = mpsc::channel::<Request<H>>(1);
        tokio::spawn(async move {
            while let Some(Request(easy2, oneshot_sender)) = request_receiver.recv().await {
                if let Err(err) = tokio::task::spawn_blocking(move || {
                    let response = perform_curl(easy2);
                    if let Err(res) = oneshot_sender.send(response) {
                        eprintln!("Warning! The receiver has been dropped. {:?}", res);
                    }
                })
                .await
                {
                    eprintln!("Error! Join Error. {:?}", err);
                }
            }
        });

        Self { request_sender }
    }

    /// This will trigger the request_reciever channel
    /// at the spawned asynchronous task to call
    /// perform_curl_multi to start communicating with
    /// the target server.
    pub async fn send_request(&self, easy2: Easy2<H>) -> Result<Easy2<H>, Error<H>>
    where
        H: Handler + Debug + Send + 'static,
    {
        let (oneshot_sender, oneshot_receiver) = oneshot::channel::<Result<Easy2<H>, Error<H>>>();
        self.request_sender
            .send(Request(easy2, oneshot_sender))
            .await?;
        oneshot_receiver.await?
    }
}

#[derive(Debug)]
pub struct Request<H: Handler + Debug + Send + 'static>(
    Easy2<H>,
    oneshot::Sender<Result<Easy2<H>, Error<H>>>,
);

/// This will perform the sending of the built Easy2
/// request to the target server.
#[cfg(feature = "multi")]
fn perform_curl<H: Handler + Debug + Send + 'static>(
    easy2: Easy2<H>,
) -> Result<Easy2<H>, Error<H>> {
    let multi = Multi::new();
    let handle = multi.add2(easy2)?;

    while multi.perform()? != 0 {
        let timeout_result = multi
            .get_timeout()
            .map(|d| d.unwrap_or_else(|| Duration::from_secs(2)));

        let timeout = match timeout_result {
            Ok(duration) => duration,
            Err(multi_error) => {
                if !multi_error.is_call_perform() {
                    return Err(Error::from(multi_error));
                }
                Duration::ZERO
            }
        };

        if !timeout.is_zero() {
            sleep(Duration::from_millis(200));
        }
    }
    multi.remove2(handle).map_err(Error::from)
}

/// This will perform the sending of the built Easy2
/// request to the target server.
#[cfg(not(feature = "multi"))]
fn perform_curl<H: Handler + Debug + Send + 'static>(
    easy2: Easy2<H>,
) -> Result<Easy2<H>, Error<H>> {
    easy2.perform().map_err(Error::from)?;
    Ok(easy2)
}

#[cfg(test)]
mod test {

    use http::StatusCode;
    use wiremock::matchers::method;
    use wiremock::matchers::path;
    use wiremock::Mock;
    use wiremock::MockServer;
    use wiremock::ResponseTemplate;

    use crate::actor::CurlActor;
    use crate::actor::Easy2;
    use crate::response_handler::ResponseHandler;
    use std::convert::TryFrom;

    async fn start_mock_server(
        node: &str,
        mock_body: String,
        mock_status_code: StatusCode,
    ) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(node))
            .respond_with(ResponseTemplate::new(mock_status_code).set_body_string(mock_body))
            .mount(&server)
            .await;
        server
    }

    #[tokio::test]
    #[cfg(not(feature = "multi"))]
    async fn test_async_requests() {
        const MOCK_BODY_RESPONSE: &str = r#"{"token":"12345"}"#;
        const MOCK_STATUS_CODE: StatusCode = StatusCode::OK;

        let server = start_mock_server(
            "/async-test",
            MOCK_BODY_RESPONSE.to_string(),
            StatusCode::OK,
        )
        .await;
        let url = format!("{}{}", server.uri(), "/async-test");

        let curl = CurlActor::new();
        let mut easy2 = Easy2::new(ResponseHandler::new());
        easy2.url(url.as_str()).unwrap();
        easy2.get(true).unwrap();

        let spawn1 = tokio::spawn(async move {
            let result = curl.send_request(easy2).await;
            let mut result = result.unwrap();
            // Test response body
            assert_eq!(
                String::from_utf8_lossy(&result.get_ref().to_owned().get_data()),
                MOCK_BODY_RESPONSE.to_string()
            );

            // Test response status code
            let status_code = result.response_code().unwrap();

            assert_eq!(
                StatusCode::from_u16(u16::try_from(status_code).unwrap()).unwrap(),
                MOCK_STATUS_CODE
            );
        });

        let curl = CurlActor::new();
        let mut easy2 = Easy2::new(ResponseHandler::new());
        easy2.url(url.as_str()).unwrap();
        easy2.get(true).unwrap();

        let spawn2 = tokio::spawn(async move {
            let result = curl.send_request(easy2).await;
            let mut result = result.unwrap();
            // Test response body
            assert_eq!(
                String::from_utf8_lossy(&result.get_ref().to_owned().get_data()),
                MOCK_BODY_RESPONSE.to_string()
            );

            // Test response status code
            let status_code = result.response_code().unwrap();
            assert_eq!(
                StatusCode::from_u16(u16::try_from(status_code).unwrap()).unwrap(),
                MOCK_STATUS_CODE
            );
        });

        let (_, _) = tokio::join!(spawn1, spawn2);
    }

    #[tokio::test]
    #[cfg(feature = "multi")]
    async fn test_async_requests_multi() {
        const MOCK_BODY_RESPONSE: &str = r#"{"token":"12345"}"#;
        const MOCK_STATUS_CODE: StatusCode = StatusCode::OK;

        let server = start_mock_server(
            "/async-test",
            MOCK_BODY_RESPONSE.to_string(),
            StatusCode::OK,
        )
        .await;
        let url = format!("{}{}", server.uri(), "/async-test");

        let curl = CurlActor::new();
        let mut easy2 = Easy2::new(ResponseHandler::new());
        easy2.url(url.as_str()).unwrap();
        easy2.get(true).unwrap();

        let spawn1 = tokio::spawn(async move {
            let result = curl.send_request(easy2).await;
            let mut result = result.unwrap();
            // Test response body
            assert_eq!(
                String::from_utf8_lossy(&result.get_ref().to_owned().get_data()),
                MOCK_BODY_RESPONSE.to_string()
            );

            // Test response status code
            let status_code = result.response_code().unwrap();

            assert_eq!(
                StatusCode::from_u16(u16::try_from(status_code).unwrap()).unwrap(),
                MOCK_STATUS_CODE
            );
        });

        let curl = CurlActor::new();
        let mut easy2 = Easy2::new(ResponseHandler::new());
        easy2.url(url.as_str()).unwrap();
        easy2.get(true).unwrap();

        let spawn2 = tokio::spawn(async move {
            let result = curl.send_request(easy2).await;
            let mut result = result.unwrap();
            // Test response body
            assert_eq!(
                String::from_utf8_lossy(&result.get_ref().to_owned().get_data()),
                MOCK_BODY_RESPONSE.to_string()
            );

            // Test response status code
            let status_code = result.response_code().unwrap();
            assert_eq!(
                StatusCode::from_u16(u16::try_from(status_code).unwrap()).unwrap(),
                MOCK_STATUS_CODE
            );
        });

        let (_, _) = tokio::join!(spawn1, spawn2);
    }
}
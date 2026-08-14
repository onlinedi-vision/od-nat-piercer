def imageTag(){
	def branchName = env.GIT_BRANCH.tokenize('/').last()
	return "registry.onlinedi.vision:5000/od-nat-piercer:v${branchName}"
}

def buildAndScanImage ={
	def tag = imageTag()

	sh 'docker buildx bake -f docker-bake.hcl --load'

	sh """
		docker run --rm \
		-v /var/run/docker.sock:/var/run/docker.sock \
		aquasec/trivy:0.36.0 image \
		--format table \
		--exit-code 1 \
		--ignore-unfixed \
		--vuln-type os,library \
		--severity CRITICAL,HIGH \
		'${tag}'
	"""
}

pipeline {
	agent any
	
	options {
		disableConcurrentBuilds()
		timeout(time: 2, unit: 'HOURS')
	}

	environment {
		CARGO_TERM_COLOR = 'always'
	}

	stages {

		stage('Test Build and Scan'){
			steps {
				script{
					buildAndScanImage()
				}
			}
		}

		stage('Push Image') {
			when{
				allOf {
					branch 'main'
					not { changeRequest() }
				}
			}

			steps {
				script{
					withDockerRegistry(
						url: 'https://registry.onlinedi.vision:5000',
						credentialsId: 'docker-registry'
					) {
						sh 'docker buildx bake -f docker-bake.hcl --push'
					}
				}
			}
		}

		stage('Deploy'){
			when{
				allOf{
					branch 'main'
					not {changeRequest()}
				}
			}

			steps {
				script {
					def tag = imageTag()

					withDockerRegistry(
						url: 'https://registry.onlinedi.vision:5000',
						credentialsId: 'docker-registry'
					){
						sh "OD_NAT_PIERCER_IMAGE='${tag}' docker compose up -d --no-build --pull always --remove-orphans"
					}
				}
			}
		}
	}

	post {
		failure{
			emailext(
				from: 'jenkins@mail.onlinedi.vision',
				subject: "Build Failed: ${env.JOB_NAME} - ${env.BUILD_NUMBER}",
				body: "Check ${env.BUILD_URL}",
				to: 'TEAM@mail.onlinedi.vision'
			)
		}
	}
}